//! Terminal painting: committing finalized rows into the native scrollback
//! (`commit_rows`), the live-zone renderers, the input/status chrome
//! assembly, and the width / control-strip text utilities shared across
//! the TUI.

use super::markdown::MdCache;
use super::theme::{dim_fg, fmt_elapsed, AGENT_FG, BADGE_BG, BADGE_FG, USER_FG};
use super::transcript::{build_history_lines, LiveRow, Row};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use unicode_width::UnicodeWidthStr;

/// Keep prose readable on wide terminals while leaving the reading column
/// left-anchored. Input and status chrome use the available terminal width.
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

/// The startup welcome line: model and tool count, and the /help pointer.
pub(super) fn welcome_text(model: &str, n_tools: usize) -> String {
    format!("tiny-agent · model {model} · {n_tools} tools · /help lists commands")
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
    if busy {
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
            format!(" {spinner} {tail}")
        }
    } else {
        " Ask TinyAgent ".to_string()
    }
}

/// Assemble the status bar in two density levels. Wide terminals get token
/// detail; medium ones keep the context-window segment (`ctx`) after the stats
/// drop out (it's the most decision-relevant number, so it outlives the stats);
/// narrow ones preserve only the model badge and broad run state. The cwd path
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
    let badge = format!(" {model} ");
    let state_seg = format!("  {state}");
    let state_style = if busy {
        // One busy color across the chrome: the input border title already
        // pops AGENT_FG, the status state segment matches it.
        Style::default().fg(AGENT_FG).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(dim_fg())
    };
    let mut spans = vec![
        Span::styled(badge, Style::default().fg(BADGE_FG).bg(BADGE_BG)),
        Span::styled(state_seg, state_style),
    ];
    let used = spans.iter().map(|s| s.content.width()).sum::<usize>();
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
/// basename cannot fit with a 2-column gap. Long paths are LEFT-truncated
/// with a leading "…" on a char boundary, so the most identifying tail —
/// the project directory — stays visible.
pub(super) fn right_path_seg(path: &str, avail: usize) -> Option<Span<'static>> {
    if path.is_empty() || avail < 12 {
        return None;
    }
    let budget = avail - 2; // minimum gap to the left segments
    let pw = path.width();
    let (text, tw) = if pw <= budget {
        (path.to_string(), pw)
    } else {
        // Basename after EITHER separator: on Windows the cwd arrives
        // backslash-delimited, where an rsplit('/')-only "basename" is the
        // WHOLE path (no split hit) — it then can't fit the budget and the
        // segment vanishes entirely, taking the cwd out of the status bar.
        let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
        if base.width() + 1 > budget {
            return None; // even "…" + basename is too wide
        }
        // Longest char-boundary suffix that fits next to the "…" marker.
        let mut w = 0;
        let mut start = 0; // 0 = whole path fits (unreachable here, defensive)
        for (i, c) in path.char_indices().rev() {
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if w + cw + 1 > budget {
                break;
            }
            w += cw;
            start = i;
        }
        if start == 0 {
            (path.to_string(), pw)
        } else {
            (format!("…{}", &path[start..]), w + 1)
        }
    };
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

/// Estimated display rows `text` occupies when wrapped at `width` — per
/// logical line, `ceil(cols / width)` with a floor of one. This is an UPPER
/// bound of what the markdown render shows (the renderer soft-joins lines
/// inside a paragraph, and joining can only shrink the row count), so the
/// split fires at least as eagerly as the viewport needs.
pub(super) fn est_display_rows(text: &str, width: usize) -> usize {
    let w = width.max(1);
    text.split('\n')
        .map(|l| {
            let cols: usize = l
                .chars()
                .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
                .sum();
            cols.div_ceil(w).max(1)
        })
        .sum()
}

/// Wrap one styled line to `width` display columns, returning one `Line` per
/// visual row. Greedy word wrap: fill a row until the next word won't fit, then
/// break at the last space; a word longer than `width` is hard-broken. A space
/// landing right after a break is dropped (cleaner left edge). Span styles are
/// preserved across breaks — a span split by a wrap becomes two spans with the
/// same style. With this, `lines.len()` equals the rendered height, so the
/// scroll range is exact.
pub(super) fn wrap_line(src: &Line, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    // Flatten to (char, style) so a wrap can split a span at any char.
    let cells: Vec<(char, Style)> = src
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(|c| (c, s.style)))
        .collect();
    let char_w = |c: char| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);

    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut row_start = 0usize; // cells index where the current row began
    let mut col = 0usize; // display width of [row_start, i)
                          // Index just past the most recent space — a valid break point as long as
                          // there's non-space content before it on this row.
    let mut last_space_after: Option<usize> = None;

    let mut i = 0usize;
    while i < cells.len() {
        let (c, _style) = cells[i];
        let w = char_w(c);
        if c == ' ' {
            if col + w > width && col > 0 {
                // The space itself would overflow: break before it and drop it.
                rows.push(line_from_cells(&cells[row_start..i]));
                row_start = i + 1;
                col = 0;
                last_space_after = None;
                i += 1;
                continue;
            }
            col += w;
            // Only treat it as a break point if real content precedes it.
            if row_start < i {
                last_space_after = Some(i + 1);
            }
            i += 1;
            continue;
        }
        if col + w > width {
            // Overflow on a non-space char. Prefer breaking at the last space.
            if let Some(after) = last_space_after {
                if after - 1 > row_start {
                    // Row = everything before the space (space dropped).
                    rows.push(line_from_cells(&cells[row_start..after - 1]));
                    row_start = after;
                    col = cells[row_start..i].iter().map(|(ch, _)| char_w(*ch)).sum();
                    last_space_after = None;
                    continue; // re-process cell i on the new row
                }
            }
            // No usable break point: hard-break before this char.
            if col > 0 {
                rows.push(line_from_cells(&cells[row_start..i]));
                row_start = i;
                col = 0;
                last_space_after = None;
                continue; // re-process cell i
            }
            // Row empty and char wider than width (or width 1): emit it anyway
            // so we make progress instead of looping forever.
        }
        col += w;
        i += 1;
    }
    if row_start < cells.len() {
        rows.push(line_from_cells(&cells[row_start..]));
    }
    if rows.is_empty() {
        rows.push(Line::default());
    }
    rows
}

/// Build a `Line` from (char, style) cells, coalescing adjacent same-style chars
/// into one span so a wrapped row doesn't explode into one span per char.
fn line_from_cells(cells: &[(char, Style)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut style = Style::default();
    for (c, st) in cells {
        if text.is_empty() {
            text.push(*c);
            style = *st;
        } else if *st == style {
            text.push(*c);
        } else {
            spans.push(Span::styled(std::mem::take(&mut text), style));
            text.push(*c);
            style = *st;
        }
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}
