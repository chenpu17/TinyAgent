//! Terminal painting: committing finalized rows into the native scrollback
//! (`commit_rows`), the live-zone renderers, the input/status chrome
//! assembly, and the width / control-strip text utilities shared across
//! the TUI.

use super::input::InputBuffer;
use super::markdown::MdCache;
use super::theme::{
    dim_fg, fmt_elapsed, fmt_tokens, keycap_bg, keycap_fg, AGENT_FG, TOOL_CALL_FG, TOOL_ERR_FG,
    USER_FG,
};
use super::transcript::{build_history_lines, LiveRow, Row};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Keep prose readable on wide terminals while leaving the reading column
/// left-anchored. Composer, queue and status share the same capped work area.
const CONTENT_MAX_WIDTH: u16 = 120;

/// Best-effort copy to the system clipboard via the platform's CLI tool. Tries
/// each known tool in turn; returns true if one accepted the text. No clipboard
/// crate dep (keeps the binary lean) — this shelling-out covers macOS (pbcopy),
/// Wayland (wl-copy), X11 (xclip/xsel), and Windows (clip). A tool that isn't
/// installed fails to spawn and we move on; if none is present, `/export` falls
/// back to writing a file.
pub(super) fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let attempts: &[(&str, &[&str])] = &[
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("clip", &[]),
    ];
    for (prog, args) in attempts {
        let mut child = match Command::new(prog)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => continue, // not installed / not on PATH → try the next
        };
        let fed = child
            .stdin
            .as_mut()
            .map(|s| s.write_all(text.as_bytes()).is_ok())
            .unwrap_or(false);
        drop(child.stdin.take()); // EOF so the tool flushes and exits
                                  // Always reap. An earlier `fed && wait()` short-circuited the wait
                                  // when the write failed (broken pipe after the tool exited early),
                                  // and Rust's `Child::drop` does NOT reap on Unix — leaking a zombie
                                  // until process exit. wait() runs unconditionally; its result still
                                  // combines with `fed` so a failed write reports failure.
                                  //
                                  // Bound the wait at 3s: a clipboard tool that holds stdin open without
                                  // exiting (e.g. xclip daemonizing in some modes) would otherwise hang
                                  // the whole TUI. The thread owns the child and reaps it whenever it
                                  // finally exits; on timeout we just stop blocking the UI.
        let exited_ok = {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(child.wait().map(|s| s.success()).unwrap_or(false));
            });
            rx.recv_timeout(std::time::Duration::from_secs(3))
                .unwrap_or(false)
        };
        let ok = fed && exited_ok;
        if ok {
            return true;
        }
    }
    false
}

/// The startup connection note. The branded empty state below it owns command
/// discovery, so this committed row only records what session was opened.
pub(super) fn welcome_text(model: &str, n_tools: usize) -> String {
    format!("session ready · {model} · {n_tools} tools")
}

/// A small raised keycap used by the welcome sheet and composer footer.
pub(super) fn keycap(label: &str) -> Span<'static> {
    Span::styled(
        format!(" {label} "),
        Style::default()
            .fg(keycap_fg())
            .bg(keycap_bg())
            .add_modifier(Modifier::BOLD),
    )
}

/// Three-row opening composition for a fresh/cleared session. The wide layout
/// is an editorial masthead rather than a paragraph of instructions: a spaced
/// wordmark with the version pinned to the same right edge as the composer, a
/// two-tone rule across the full work area, and one concise promise. Keys are
/// not repeated here — the composer's own bottom border carries the legend two
/// rows below. Render-only: hints never become fake conversation rows or
/// `/export` noise.
pub(super) fn empty_state_lines(width: usize) -> Vec<Line<'static>> {
    let mut brand_spans = vec![
        Span::styled(
            "◆",
            Style::default().fg(AGENT_FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " T I N Y A G E N T",
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    let brand_left = brand_spans.iter().map(|s| s.content.width()).sum::<usize>();
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    if width >= 80 && brand_left + version.width() + 2 <= width {
        brand_spans.push(Span::raw(" ".repeat(width - brand_left - version.width())));
        brand_spans.push(Span::styled(version, Style::default().fg(dim_fg())));
    }
    let brand = Line::from(brand_spans);

    let accent = width.min(12);
    let rule = Line::from(vec![
        Span::styled("━".repeat(accent), Style::default().fg(AGENT_FG)),
        Span::styled(
            "─".repeat(width.saturating_sub(accent)),
            Style::default().fg(dim_fg()),
        ),
    ]);
    let tagline = Line::from(Span::styled(
        if width >= 42 {
            "A small agent for real terminal work."
        } else {
            "Ask. Build. Debug."
        },
        Style::default(),
    ));
    [brand, rule, tagline]
        .into_iter()
        .map(|line| truncate_line(line, width))
        .collect()
}

/// Key legend drawn inside the composer's bottom border. Keycaps match the
/// slash menu; narrow frames keep only send + help. This is the only key
/// legend on an empty screen — the welcome sheet deliberately does not repeat
/// it.
pub(super) fn composer_footer(busy: bool, width: usize) -> Option<Line<'static>> {
    if busy {
        return None;
    }
    let full = Line::from(vec![
        Span::raw(" "),
        keycap("Enter"),
        Span::styled(" send  ", Style::default().fg(dim_fg())),
        keycap("/"),
        Span::styled(" commands  ", Style::default().fg(dim_fg())),
        keycap("Ctrl-J"),
        Span::styled(" newline  ", Style::default().fg(dim_fg())),
        keycap("/help"),
        Span::raw(" "),
    ]);
    let short = Line::from(vec![
        Span::raw(" "),
        keycap("Enter"),
        Span::styled(" send  ", Style::default().fg(dim_fg())),
        keycap("/help"),
        Span::raw(" "),
    ]);
    if full.width() <= width {
        Some(full.right_aligned())
    } else if short.width() <= width {
        Some(short.right_aligned())
    } else {
        None
    }
}

/// The one color for the bottom chrome (input border + its title): AGENT_FG
/// while a run is in flight so the frame reads as a single busy unit, the
/// idle color otherwise. One busy color across chrome AND status bar.
pub(super) fn chrome_fg(busy: bool) -> Color {
    if busy {
        AGENT_FG
    } else {
        USER_FG
    }
}

/// Input border title for idle state or the current in-flight run. `width` is
/// the room the border row offers (input box width minus the two corners).
/// ratatui clips an over-long title from the RIGHT — silently dropping the
/// elapsed time, the cancel hint, and the queue count, exactly the actionable
/// segments — so a busy title that would overflow degrades here instead: the
/// activity name goes first (the least actionable part), keeping spinner +
/// elapsed + cancel + queue.
pub(super) fn build_input_title(
    busy: bool,
    spinner: char,
    activity: &str,
    secs: u64,
    queued: usize,
    width: usize,
) -> String {
    let title = if busy {
        let queue = if queued == 0 {
            String::new()
        } else {
            format!(" · {queued} queued")
        };
        let tail = format!("{} · Ctrl-C cancel{queue} ", fmt_elapsed(secs));
        let full = format!(" {spinner} {activity}… · {tail}");
        if full.width() <= width {
            full
        } else {
            // Shed decoration before the actionable cancellation hint. Queue
            // details have their own live preview, so they are the next segment
            // to go; elapsed time disappears only when the hint cannot fit.
            let no_queue = format!(" {spinner} {} · Ctrl-C cancel ", fmt_elapsed(secs));
            let cancel_only = format!(" {spinner} Ctrl-C cancel ");
            let degraded = format!(" {spinner} {tail}");
            if degraded.width() <= width {
                degraded
            } else if no_queue.width() <= width {
                no_queue
            } else {
                cancel_only
            }
        }
    } else {
        " ◆ MESSAGE ".to_string()
    };
    truncate_display(&title, width)
}

/// Input border title for the `failed` phase, already truncated to `width`.
/// `/resume` only acts while a paused queue is holding messages — with nothing
/// held it would answer "[no paused queue]" — so the hint names the action
/// that exists for the current state. Narrow titles keep the shorter form
/// rather than losing the right-hand hint to ratatui's right clip.
pub(super) fn failed_title(held: usize, width: usize) -> String {
    let (full, short) = if held > 0 {
        (
            " failed · /resume continues the held queue ",
            " failed · /resume ",
        )
    } else {
        (" failed · type a message to retry ", " failed ")
    };
    if full.width() <= width {
        full.to_string()
    } else {
        truncate_display(short, width)
    }
}

/// Cumulative token segment for the status bar. Empty until the first billed
/// turn: "0 in / 0 out" on a fresh session is pure noise. Cache counters count
/// as activity even when the plain counters have not moved yet.
pub(super) fn status_stats(
    in_tokens: u64,
    out_tokens: u64,
    cache_create: u64,
    cache_read: u64,
) -> String {
    if in_tokens == 0 && out_tokens == 0 && cache_create == 0 && cache_read == 0 {
        return String::new();
    }
    let cache_seg = if cache_read > 0 || cache_create > 0 {
        format!(
            "   cache {} w {} r",
            fmt_tokens(cache_create),
            fmt_tokens(cache_read)
        )
    } else {
        String::new()
    };
    format!(
        "   {} in  /  {} out{cache_seg}",
        fmt_tokens(in_tokens),
        fmt_tokens(out_tokens)
    )
}

/// Assemble the status bar in two density levels. Wide terminals get token
/// detail; medium ones keep the context-window segment (`ctx`) after the stats
/// drop out (it's the most decision-relevant number, so it outlives the stats);
/// narrow ones preserve only the model label and broad run state. The cwd path
/// pins to the right edge in whatever room the left side leaves (it is the
/// first thing narrow terminals lose). Keyboard help lives behind `/help`.
// The bar is assembled from independent segments; grouping them into a
// struct would only shuffle the same fields to every call site.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_status_line(
    model: &str,
    state: &str,
    busy: bool,
    ctx: Option<(&str, Style)>,
    plan: Option<(&str, Style)>,
    stats: &str,
    path: &str,
    width: usize,
) -> Line<'static> {
    let model_label = format!(" ◆ {model}");
    let (state_mark, state_label) = match state {
        "idle" => ("●", "ready"),
        "running" => ("◆", "running"),
        "cancelling" => ("!", "cancelling"),
        "failed" => ("✗", "failed"),
        _ => ("·", state),
    };
    let state_seg = format!("  {state_mark} {state_label}");
    let state_style = match state {
        "cancelling" => Style::default()
            .fg(TOOL_CALL_FG)
            .add_modifier(Modifier::BOLD),
        "failed" => Style::default()
            .fg(TOOL_ERR_FG)
            .add_modifier(Modifier::BOLD),
        "idle" | "running" => Style::default().fg(AGENT_FG).add_modifier(Modifier::BOLD),
        _ if busy => Style::default().fg(AGENT_FG).add_modifier(Modifier::BOLD),
        _ => Style::default().fg(dim_fg()),
    };
    let mut spans = vec![
        Span::styled(
            model_label,
            Style::default().fg(USER_FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(state_seg, state_style),
    ];
    let used = spans.iter().map(|s| s.content.width()).sum::<usize>();
    if used > width {
        // Last-resort density: the broad state is more actionable than the
        // model label. ratatui must never be left to right-clip it silently.
        return Line::from(Span::styled(
            truncate_display(state_label, width),
            state_style,
        ));
    }
    // Optional segments in SURVIVAL order (highest priority first): ctx, plan,
    // stats. Each is added only if it AND every higher-priority segment fit, so
    // narrowing drops right-to-left — stats first, then plan, then ctx (ctx
    // survives longest, matching the documented "ctx 保留到最后"). `plan` is
    // None unless the agent used `todo`, so a no-plan run is byte-identical to
    // before the segment existed.
    let mut opt: Vec<(String, Style)> = Vec::new();
    if let Some((t, s)) = ctx {
        opt.push((format!(" · {t}"), s));
    }
    if let Some((t, s)) = plan {
        opt.push((t.to_string(), s));
    }
    opt.push((stats.to_string(), Style::default().fg(dim_fg())));
    let mut prefix_w = 0usize;
    for (text, style) in &opt {
        prefix_w += text.width();
        if used + prefix_w <= width {
            spans.push(Span::styled(text.clone(), *style));
        } else {
            // Lower-priority segments won't fit either — stop.
            break;
        }
    }
    // Right-aligned cwd path in the room the left side left over.
    let left_w = spans.iter().map(|s| s.content.width()).sum::<usize>();
    if let Some(seg) = right_path_seg(path, width.saturating_sub(left_w)) {
        spans.push(seg);
    }
    Line::from(spans)
}

/// Right-aligned cwd path segment for the status bar. Returns None when the
/// leftover after the left segments is under 12 columns (anything less is
/// useless truncation), when the path is empty, or when even "…" + the
/// basename cannot fit with its separator and a 2-column gap. Long paths keep
/// the longest component-aligned suffix with a leading "…", so the identifying
/// project directories survive without half a directory name.
pub(super) fn right_path_seg(path: &str, avail: usize) -> Option<Span<'static>> {
    if path.is_empty() || avail < 12 {
        return None;
    }
    // Even on a wide display a full absolute path becomes visual noise. Keep
    // the identifying tail within a fixed 48-cell budget and right-align it.
    let budget = (avail - 2).min(48);
    let pw = path.width();
    let text = if pw <= budget {
        path.to_string()
    } else {
        // Ellipsize at component boundaries, keeping the LONGEST trailing
        // run that fits. A character-level suffix produced noisy fragments
        // such as `…de-code/TinyAgent`; `…/TinyAgent` is easier to scan.
        // Either separator works, including mixed-separator Windows paths.
        let mut shortened = None;
        for (i, c) in path.char_indices().rev() {
            if !matches!(c, '/' | '\\') {
                continue;
            }
            let candidate = format!("…{}", &path[i..]);
            if candidate.width() <= budget {
                shortened = Some(candidate);
            } else {
                break;
            }
        }
        shortened?
    };
    let tw = text.width();
    let pad = avail - tw;
    Some(Span::styled(
        format!("{}{text}", " ".repeat(pad)),
        Style::default().fg(dim_fg()),
    ))
}

/// Wrap logical history lines to the render width. The inline model commits
/// finalized lines to native scrollback, so there is no per-visual-line row
/// map to maintain — wrapping is purely about fitting text to the width.
pub(super) fn wrap_history(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    lines.iter().flat_map(|l| wrap_line(l, width)).collect()
}

/// Render width for transcript text: the live viewport spans the terminal, so
/// there is no horizontal inset, but prose is still capped at a readable max.
pub(super) fn content_width(term_w: u16) -> usize {
    (term_w as usize).min(CONTENT_MAX_WIDTH as usize).max(1)
}

/// Soft-wrapped composer rendering and its display-cell caret. Kept pure so
/// cell-grid tests can assert the exact same geometry the app draws.
pub(super) struct ComposerLayout {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) caret_row: usize,
    pub(super) caret_col: usize,
}

pub(super) fn layout_composer(
    input: &InputBuffer,
    width: usize,
    prompt: &str,
    prompt_style: Style,
) -> ComposerLayout {
    let mut lines = Vec::new();
    let mut caret_row = 0usize;
    let mut caret_col = 0usize;
    for (logical_row, text) in input.lines_view().iter().enumerate() {
        let prefix = if logical_row == 0 { prompt } else { "  " };
        let prefix_style = if logical_row == 0 {
            prompt_style
        } else {
            Style::default()
        };
        let source = Line::from(vec![
            Span::styled(prefix.to_string(), prefix_style),
            Span::raw(text.clone()),
        ]);
        let caret = (logical_row == input.row)
            .then(|| prefix.chars().count() + text[..input.col].chars().count());
        let (wrapped, caret_pos) = wrap_line_with_caret(&source, width, caret);
        if let Some((row, col)) = caret_pos {
            caret_row = lines.len() + row;
            caret_col = col;
        }
        lines.extend(wrapped);
    }
    ComposerLayout {
        lines,
        caret_row,
        caret_col,
    }
}

/// First composer visual row to display in a fixed-height input box. Keeping
/// this calculation beside the layout makes cell-grid tests exercise the same
/// vertical scrolling rule as the app.
pub(super) fn composer_scroll(caret_row: usize, inner_rows: usize) -> u16 {
    caret_row
        .saturating_sub(inner_rows.saturating_sub(1))
        .min(u16::MAX as usize) as u16
}

/// One-line queue status outside the transcript: pending input must never
/// hold back tool results or streaming text from the commit watermark.
pub(super) fn queue_preview_line(
    queued: &std::collections::VecDeque<String>,
    paused: bool,
    width: usize,
) -> Option<Line<'static>> {
    let first = queued.front()?;
    let state = if paused { "paused" } else { "queued" };
    let suffix = if queued.len() > 1 {
        format!(" · +{}", queued.len() - 1)
    } else {
        String::new()
    };
    let action = if paused { " · /resume" } else { "" };
    let head = truncate_display(
        &format!("⇢ {} {state}{suffix}{action} · ", queued.len()),
        width,
    );
    let remaining = width.saturating_sub(head.width());
    let preview = truncate_display(&first.replace('\n', " ↵ "), remaining);
    Some(Line::from(vec![
        Span::styled(head, Style::default().fg(USER_FG)),
        Span::styled(preview, Style::default().fg(dim_fg())),
    ]))
}

pub(super) fn commit_rows<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    rows: &[Row],
    md_cache: &mut MdCache,
    width: usize,
    trailing_spacer: bool,
) -> Result<(), B::Error> {
    let lines = build_history_lines(rows, md_cache, width, LiveRow::None, false, trailing_spacer);
    let wrapped = wrap_history(&lines, width);
    if wrapped.is_empty() {
        return Ok(());
    }
    // Cap at u16::MAX: insert_before's height is a u16, and `y as u16` in the
    // loop would wrap past 65,535 rendered lines — silently reserving too few
    // rows while the loop overwrites past them. A single commit reaching that
    // size is not realistic (a ~65k-line turn), but capping drops the
    // unreachable tail instead of corrupting the layout.
    let n = wrapped.len().min(u16::MAX as usize);
    let h = n as u16;
    term.insert_before(h, |buf: &mut Buffer| {
        for (y, line) in wrapped[..n].iter().enumerate() {
            // set_line writes the line's spans from x=0, truncating at the
            // buffer's width — which equals `width` (or wider), so the
            // pre-wrapped lines never overflow.
            buf.set_line(0, y as u16, line, width as u16);
        }
    })?;
    Ok(())
}

/// Top-anchored render for a menu (the slash popup): line 0 sits at the top of
/// `rect`. The caller windows `lines` down to the zone height first (see
/// `slash_popup_lines`), keeping the selection on-screen — so this just paints
/// top-down with no scroll, and the highlighted row can never be the one scrolled
/// out of view.
pub(super) fn render_zone_top(f: &mut Frame, rect: Rect, lines: &[Line<'static>]) {
    if rect.height == 0 || lines.is_empty() {
        return;
    }
    f.render_widget(Paragraph::new(lines.to_vec()), rect);
}

/// Bottom-align `lines` inside `rect` (the transcript zone above the input box):
/// the last line sits flush against the input box. Used for the thinking
/// placeholder and the in-flight streaming row. When the content FITS it is
/// padded down to the bottom of the zone; when it OVERFLOWS, a dim `↑ +N`
/// indicator caps the top and the tail fills the rest — so it's clear the live
/// answer is taller than the band (the full text commits to scrollback once the
/// turn ends, and is scrollable there).
pub(super) fn render_zone_tail(f: &mut Frame, rect: Rect, lines: &[Line<'static>]) {
    if rect.height == 0 || lines.is_empty() {
        return;
    }
    let n = lines.len();
    let h = rect.height as usize;
    if n <= h {
        // Fits: pad the top so the last line lands flush above the input box.
        let pad = (h - n) as u16;
        let body = if pad > 0 {
            let [_, b] =
                Layout::vertical([Constraint::Length(pad), Constraint::Min(0)]).areas(rect);
            b
        } else {
            rect
        };
        f.render_widget(Paragraph::new(lines.to_vec()), body);
        return;
    }
    // Overflow: a one-row truncation hint at the top, the tail below it. The
    // hint occupies one of the h rows, so the content rows left are h-1 — the
    // hidden count is what the tail CAN'T show.
    let hidden = n - (h - 1);
    let hint = Line::from(Span::styled(
        format!(
            "↑ +{hidden} line{} above",
            if hidden == 1 { "" } else { "s" }
        ),
        Style::default().fg(dim_fg()).add_modifier(Modifier::ITALIC),
    ));
    let [hint_rect, tail_rect] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(rect);
    f.render_widget(Paragraph::new(hint), hint_rect);
    let take = (tail_rect.height as usize).min(n);
    f.render_widget(Paragraph::new(lines[n - take..].to_vec()), tail_rect);
}

/// Strip control characters (escape sequences, NUL, …) from external text
/// before it enters the row list — tool output or model text containing ESC /
/// CSI sequences would otherwise corrupt the TUI frame. Newlines and tabs are
/// kept (both are legitimate content here). Applied once at the row-entry
/// points, never on the render path.
pub(super) fn strip_controls(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

/// strip_controls for every string inside a JSON value (tool-call input), so
/// both the collapsed summary and the expanded pretty-print come out clean
/// without re-filtering on the render path.
pub(super) fn strip_controls_json(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => serde_json::Value::String(strip_controls(s)),
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(strip_controls_json).collect())
        }
        serde_json::Value::Object(o) => serde_json::Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), strip_controls_json(v)))
                .collect(),
        ),
        _ => v.clone(),
    }
}

/// Estimated display rows `text` occupies under the same greedy word-wrap
/// used by [`wrap_line`]. Markdown may soft-join adjacent source lines, so the
/// streaming splitter still treats this as an eager threshold, but hard wraps,
/// CJK width and word boundaries no longer use a second, incompatible ruler.
pub(super) fn est_display_rows(text: &str, width: usize) -> usize {
    text.split('\n')
        .map(|line| {
            let cells = layout_cells(line, &[]);
            wrap_ranges(&cells, width, 0, 0).len()
        })
        .sum()
}

/// Source byte offsets at which each wrapped display row starts. The splitter
/// uses this to cut on exactly the boundaries produced by `wrap_line`, never
/// inside a grapheme cluster such as a ZWJ emoji sequence.
pub(super) fn display_row_starts(text: &str, width: usize) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut line_start = 0usize;
    for line in text.split('\n') {
        let cells = layout_cells(line, &[]);
        for (start, _) in wrap_ranges(&cells, width, 0, 0) {
            let at = cells.get(start).map(|cell| cell.byte_start).unwrap_or(0);
            starts.push(line_start + at);
        }
        line_start += line.len() + 1;
    }
    if starts.is_empty() {
        starts.push(0);
    }
    starts
}

/// Cell-aware styled-line truncation. Preserves span styling and grapheme
/// boundaries; every positive width reserves its last cell for `…` on overflow.
pub(super) fn truncate_line(mut line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width {
        return line;
    }
    if width == 0 {
        return Line::default();
    }
    let budget = width - 1;
    let mut used = 0usize;
    let mut stopped = false;
    let mut spans: Vec<Span<'static>> = Vec::new();
    for span in line.spans.drain(..) {
        let style = span.style;
        let mut text = String::new();
        for grapheme in span.content.graphemes(true) {
            let cell_w = UnicodeWidthStr::width(grapheme);
            if used.saturating_add(cell_w) > budget {
                stopped = true;
                break;
            }
            used = used.saturating_add(cell_w);
            text.push_str(grapheme);
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, style));
        }
        if stopped {
            break;
        }
    }
    let ellipsis_style = spans.last().map(|span| span.style).unwrap_or_default();
    spans.push(Span::styled("…", ellipsis_style));
    line.spans = spans;
    line
}

/// Truncate plain text to a display-cell budget, adding an ellipsis when
/// content was removed. Every caller uses this instead of byte slicing so CJK
/// and emoji never produce invalid UTF-8 or half-visible columns.
pub(super) fn truncate_display(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let budget = width - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for grapheme in text.graphemes(true) {
        let w = UnicodeWidthStr::width(grapheme);
        if used + w > budget {
            break;
        }
        out.push_str(grapheme);
        used += w;
    }
    out.push('…');
    out
}

/// Continuation prefix inferred from the line's existing visual language.
/// Role markers become two blank cells; existing indentation is retained;
/// code/tool guides (`▎ ` / `│ `) repeat on every visual row. System markers
/// become an equivalent hanging indent. This lets the FINAL wrap pass restore
/// gutters without every renderer pre-wrapping its own text.
fn continuation_cells(src: &Line) -> Vec<(String, Style)> {
    let cells: Vec<(char, Style)> = src
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(|c| (c, s.style)))
        .collect();
    let leading = cells.iter().take_while(|(c, _)| *c == ' ').count();
    if leading > 0 {
        let marker = cells.get(leading).map(|(c, _)| *c);
        let take = match marker {
            Some('▎' | '│') => (leading + 2).min(cells.len()),
            Some('·') => leading + 2,
            _ => leading,
        };
        return cells
            .iter()
            .take(take)
            .map(|(c, style)| {
                let c = if matches!(marker, Some('·')) {
                    ' '
                } else {
                    *c
                };
                (c.to_string(), *style)
            })
            .collect();
    }
    if cells
        .first()
        .is_some_and(|(c, _)| matches!(c, '❯' | '◆' | '●' | '✻' | '·'))
    {
        return cells
            .iter()
            .take(2)
            .map(|(_, style)| (' '.to_string(), *style))
            .collect();
    }
    Vec::new()
}

/// A narrow input/transcript still needs at least one content column. Degrade
/// the continuation gutter instead of letting it occupy the whole row and
/// clip every character that follows it.
fn fit_continuation(cells: Vec<(String, Style)>, width: usize) -> Vec<(String, Style)> {
    let budget = width.saturating_sub(1);
    let mut used = 0usize;
    cells
        .into_iter()
        .take_while(|(text, _)| {
            let w = UnicodeWidthStr::width(text.as_str());
            if used + w > budget {
                return false;
            }
            used += w;
            true
        })
        .collect()
}

/// Wrap one styled line to `width` display columns. Greedy word wrap with a
/// hard-break fallback for over-wide words; styles and line-level style survive
/// every split. Continuation rows receive the inferred indent/gutter, and are
/// measured against the correspondingly smaller content width.
pub(super) fn wrap_line(src: &Line, width: usize) -> Vec<Line<'static>> {
    wrap_line_with_caret(src, width, None).0
}

/// One terminal-shaped grapheme and its source mapping. `char_*` offsets serve
/// the composer caret; `byte_start` serves streaming soft cuts. Width is the
/// width of the WHOLE grapheme, so ZWJ emoji and variation selectors cannot be
/// split or counted as several unrelated characters.
#[derive(Clone)]
struct LayoutCell<'a> {
    text: &'a str,
    style: Style,
    char_start: usize,
    char_end: usize,
    byte_start: usize,
    width: usize,
}

fn layout_cells<'a>(text: &'a str, styles: &[Style]) -> Vec<LayoutCell<'a>> {
    let mut cells = Vec::new();
    let mut char_start = 0usize;
    for (byte_start, grapheme) in text.grapheme_indices(true) {
        let char_end = char_start + grapheme.chars().count();
        cells.push(LayoutCell {
            text: grapheme,
            style: styles.get(char_start).copied().unwrap_or_default(),
            char_start,
            char_end,
            byte_start,
            width: UnicodeWidthStr::width(grapheme),
        });
        char_start = char_end;
    }
    cells
}

/// `wrap_line` plus an optional caret expressed as a flattened CHARACTER
/// index. Returned caret coordinates are `(visual_row, display_column)` within
/// the returned lines. The composer uses this so wrapping and cursor placement
/// can never disagree about wide characters or dropped wrap spaces.
fn wrap_line_with_caret(
    src: &Line,
    width: usize,
    caret: Option<usize>,
) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
    let width = width.max(1);
    let text: String = src.spans.iter().map(|s| s.content.as_ref()).collect();
    let styles: Vec<Style> = src
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(|_| s.style))
        .collect();
    let cells = layout_cells(&text, &styles);
    let raw_continuation = continuation_cells(src);
    let prefix_cells = raw_continuation.len();
    let continuation = fit_continuation(raw_continuation, width);
    let cont_w: usize = continuation
        .iter()
        .map(|(text, _)| UnicodeWidthStr::width(text.as_str()))
        .sum();
    let ranges = wrap_ranges(&cells, width, cont_w, prefix_cells);
    let total_chars = cells.last().map(|cell| cell.char_end).unwrap_or(0);
    let mut rows = Vec::with_capacity(ranges.len());
    let mut caret_pos = None;
    for (row_idx, (start, end)) in ranges.iter().copied().enumerate() {
        let mut row_cells = if row_idx == 0 {
            Vec::new()
        } else {
            continuation.clone()
        };
        row_cells.extend(
            cells[start..end]
                .iter()
                .map(|cell| (cell.text.to_string(), cell.style)),
        );
        let mut line = line_from_cells(&row_cells);
        line.style = src.style;
        rows.push(line);

        let Some(at) = caret else { continue };
        if caret_pos.is_some() {
            continue;
        }
        let row_char_start = cells.get(start).map(|cell| cell.char_start).unwrap_or(0);
        let row_char_end = if start < end {
            cells[end - 1].char_end
        } else {
            row_char_start
        };
        let next_char_start = ranges
            .get(row_idx + 1)
            .and_then(|(next, _)| cells.get(*next))
            .map(|cell| cell.char_start)
            .unwrap_or(total_chars);
        // A word-wrap space is omitted from the rendered rows. The caret
        // BEFORE that space belongs to the previous row; the caret AFTER it
        // belongs at the next row's start. Hard-wrap boundaries intentionally
        // choose the next row, matching normal terminal editing behavior.
        let before_dropped_space = at == row_char_end && next_char_start > row_char_end;
        let in_row = at >= row_char_start
            && (at < row_char_end
                || before_dropped_space
                || start == end && at == row_char_start
                || row_idx + 1 == ranges.len());
        if !in_row {
            continue;
        }
        let mut col = if row_idx == 0 { 0 } else { cont_w };
        for cell in &cells[start..end] {
            if at >= cell.char_end {
                col += cell.width;
            } else {
                break;
            }
        }
        let cap = width;
        if col >= cap && row_idx + 1 < ranges.len() {
            // There is no cell beyond a full row; use the next row's start.
            continue;
        }
        caret_pos = Some((row_idx, col));
    }
    // At an exact full-width end, place the caret on a fresh continuation row
    // rather than one cell beyond the Paragraph's inner rectangle.
    if caret.is_some_and(|at| at == total_chars) {
        if let Some((_, col)) = caret_pos {
            if col >= width {
                let mut blank = line_from_cells(&continuation);
                blank.style = src.style;
                rows.push(blank);
                caret_pos = Some((rows.len() - 1, cont_w));
            }
        }
    }
    (rows, caret_pos)
}

/// Half-open ranges into grapheme `cells`, one per visual row. `cont_width`
/// reserves room for the continuation prefix on every row after the first;
/// `prefix_cells` protects a role/gutter prefix from becoming a word-break
/// candidate. Break spaces are omitted from both adjacent ranges.
fn wrap_ranges(
    cells: &[LayoutCell<'_>],
    width: usize,
    cont_width: usize,
    prefix_cells: usize,
) -> Vec<(usize, usize)> {
    let width = width.max(1);
    if cells.is_empty() {
        return vec![(0, 0)];
    }
    let mut rows = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut col = 0usize;
    let mut last_space_after: Option<usize> = None;
    while i < cells.len() {
        let cell = &cells[i];
        let w = cell.width;
        let cap = if rows.is_empty() {
            width
        } else {
            width.saturating_sub(cont_width).max(1)
        };
        if cell.text == " " {
            if col + w > cap && col > 0 {
                rows.push((start, i));
                start = i + 1;
                i += 1;
                col = 0;
                last_space_after = None;
                continue;
            }
            col += w;
            if start < i && i + 1 > prefix_cells {
                last_space_after = Some(i + 1);
            }
            i += 1;
            continue;
        }
        if col + w > cap {
            if let Some(after) = last_space_after {
                if after - 1 > start {
                    rows.push((start, after - 1));
                    start = after;
                    col = cells[start..i].iter().map(|cell| cell.width).sum();
                    last_space_after = cells[start..i]
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(_, cell)| cell.text == " ")
                        .map(|(j, _)| start + j + 1);
                    continue;
                }
            }
            if col > 0 {
                rows.push((start, i));
                start = i;
                col = 0;
                last_space_after = None;
                continue;
            }
            // A wide grapheme in a one-cell viewport still gets emitted:
            // splitting it would corrupt the glyph, and forcing progress is
            // preferable to an infinite wrap loop.
        }
        col += w;
        i += 1;
    }
    if start < cells.len() {
        rows.push((start, cells.len()));
    }
    if rows.is_empty() {
        rows.push((0, cells.len()));
    }
    rows
}

/// Build a `Line` from owned grapheme/style cells, coalescing adjacent
/// same-style cells so a wrapped row doesn't explode into one span per glyph.
fn line_from_cells(cells: &[(String, Style)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut style = Style::default();
    for (cell, st) in cells {
        if text.is_empty() {
            text.push_str(cell);
            style = *st;
        } else if *st == style {
            text.push_str(cell);
        } else {
            spans.push(Span::styled(std::mem::take(&mut text), style));
            text.push_str(cell);
            style = *st;
        }
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}
