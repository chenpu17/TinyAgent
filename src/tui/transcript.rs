//! Conversation rows: the `Row` model and everything that builds or mutates
//! it — history line rendering, streamed-delta appenders, the incremental
//! streaming split, the `/export` transcript dump, and the resume-transcript
//! rebuild. Pure row/text logic; terminal painting lives in `render`.

use super::app::INLINE_HEIGHT;
use super::markdown::{
    ansi_guided_lines, ansi_plain, fence_next, fence_step, line_to_owned, render_markdown,
    FenceLine, MdCache,
};
use super::render::{
    display_row_starts, est_display_rows, strip_controls, strip_controls_json, wrap_line,
};
use super::theme::{
    dim_fg, fmt_tool_elapsed, AGENT_FG, THINKING_FG, TOOL_CALL_FG, TOOL_ERR_FG, TOOL_OK_FG, USER_FG,
};
use crate::provider::{Block, Message, Role};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::time::Instant;
use unicode_segmentation::UnicodeSegmentation;

/// Body indent beneath the standalone conversation role headers.
pub(super) const ROLE_BODY_INDENT: usize = 2;
pub(super) const AGENT_MARK: &str = "◆ ";
const TOOL_INDENT: usize = 4;
pub(super) const TOOL_HEAD_MARK: &str = "    ▎ ";

/// Severity of a `Row::System` notice — drives its color (info dim, warn
/// yellow, error red). The text keeps its `[warn]`/`[error]`-style prefix, so
/// the plain-text /export transcript stays self-describing without the level.
/// pub(crate): rides along with the pub(crate) `Row::System` variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SysLevel {
    Info,
    Warn,
    Error,
}

/// One rendered row in the message history. Clone: `/out` reprints a past
/// ToolResult expanded, which clones the row out of the committed history.
/// pub(crate): crosses into main.rs as the resume-transcript payload.
#[derive(Clone, Debug)]
pub(crate) enum Row {
    User(String),
    Assistant(String),
    /// Continuation of the previous Assistant message — a chunk split off
    /// the streaming row by `split_streaming_row` once it outgrew the live
    /// viewport (incremental commit). Renders like Assistant but WITHOUT
    /// the `◆` icon (the message head already has it) — one reply must not
    /// read as N messages. Merges with append_delta like Assistant does.
    /// `tight: true` marks a MID-PARAGRAPH chunk (line-level cut of a long
    /// single paragraph): it renders flush against the previous row — no
    /// spacer blank, the transcript joins it with `\n` — so a paragraph
    /// split across chunks still reads as one block.
    AssistantCont {
        text: String,
        tight: bool,
    },
    /// Reasoning trace (ThinkingDelta): rendered as plain dim-italic prose
    /// with a `✻ ` marker — deliberately NOT markdown/MdCache so it can
    /// never read like the answer (the old code folded the trace into the
    /// Assistant row and the two merged into one blob). A sub-agent's trace
    /// carries a `[name] ` prefix in the text at creation. Empty traces
    /// (whitespace-only) render nothing. Long final traces fold automatically;
    /// `tight` only controls spacing between adjacent trace rows. New streaming
    /// rows always use `false` because Thinking is no longer incrementally split.
    Thinking {
        text: String,
        tight: bool,
    },
    /// Explicit `/thinking` reprint. Kept separate from the live/final row so
    /// adding folded presentation never changes the streaming state machine.
    ThinkingFull(String),
    /// Tool call: collapsed shows a compact one-line arg summary; expanded
    /// pretty-prints the full input JSON. In the inline model rows commit to
    /// scrollback at their default state, so `expanded` stays at its initial
    /// `false` (a one-line summary) — there's no in-app cursor to toggle it
    /// post-commit. `collapsed` caches the compact arg summary at creation so
    ToolCall {
        name: String,
        input: serde_json::Value,
        collapsed: String,
        expanded: bool,
        started: Instant,
    },
    /// Tool result: collapsed shows a truncated preview; expanded shows the
    /// full output (which can be large, but participates in normal scrolling).
    /// `collapsed` caches the preview at creation (like ToolCall does) so the
    /// renderer doesn't re-truncate the content every frame. `elapsed` is the
    /// pre-formatted duration of the matching call ("1.2s"), None when no
    /// same-name ToolCall row was found (e.g. a resumed session).
    ToolResult {
        name: String,
        content: String,
        collapsed: String,
        is_error: bool,
        expanded: bool,
        elapsed: Option<String>,
    },
    System {
        text: String,
        level: SysLevel,
    },
}

impl Row {
    /// Convenience constructors so the many System-row sites name a level
    /// without repeating the struct literal.
    fn sys(text: impl Into<String>, level: SysLevel) -> Row {
        Row::System {
            text: text.into(),
            level,
        }
    }
    pub(super) fn sys_info(text: impl Into<String>) -> Row {
        Row::sys(text, SysLevel::Info)
    }
    pub(super) fn sys_warn(text: impl Into<String>) -> Row {
        Row::sys(text, SysLevel::Warn)
    }
    pub(super) fn sys_error(text: impl Into<String>) -> Row {
        Row::sys(text, SysLevel::Error)
    }
}

/// AgentEvent::CodeLog payload → transcript row. The payload stays BARE:
/// the System-Info renderer (build_history_lines) and render_transcript
/// each draw their own `·` marker, so a prefix baked in here would render
/// doubled ("· · line") on screen and in /export.
pub(super) fn code_log_row(line: String) -> Row {
    Row::sys_info(line)
}

/// Render the conversation rows as plain text for `/export` — a readable
/// transcript mirroring the on-screen layout, minus styling/wrapping. Empty
/// assistant rows (left while a turn is mid-stream) are skipped so the dump
/// has no bare "Tiny" headers with no body.
///
/// Separators are chosen by the FOLLOWING block: a tight chunk (line-level
/// cut mid-paragraph) folds into the previous block with a single `\n`, so
/// concatenated chunks reassemble the original text; everything else starts
/// after a blank line. A tight chunk cut inside a code fence begins with a
/// synthetic opener — the fence's marker char at the opener's length, ``` or
/// ~~~ alike (see `split_streaming_row`) — so it renders as code on its own;
/// stripped here, the model never wrote that line. Corner case: a
/// model-emitted bare fence opener landing exactly on a cut line loses that
/// line in the dump (readability export, not an archive).
pub(super) fn render_transcript(rows: &[Row]) -> String {
    let mut out = String::new();
    for row in rows {
        // (block header, body, mid-block?) — a continuation chunk carries no
        // header of its own (the head chunk has it).
        let block: (String, &str, bool) = match row {
            Row::User(t) => ("You\n".into(), t.as_str(), false),
            Row::Assistant(t) if t.is_empty() => continue,
            Row::Assistant(t) => ("Tiny\n".into(), t.as_str(), false),
            Row::AssistantCont { text: t, .. } if t.is_empty() => continue,
            Row::AssistantCont { text: t, tight } => {
                let body = if *tight {
                    strip_synthetic_opener(t)
                } else {
                    t.as_str()
                };
                (String::new(), body, *tight)
            }
            Row::Thinking { text: t, .. } if t.trim().is_empty() => continue,
            Row::Thinking { text: t, tight } => ("✻ Thinking\n".into(), t.as_str(), *tight),
            Row::ThinkingFull(t) if t.trim().is_empty() => continue,
            Row::ThinkingFull(t) => ("✻ Thinking (full)\n".into(), t.as_str(), false),
            Row::ToolCall { name, input, .. } => (format!("→ {name}: {input}\n"), "", false),
            Row::ToolResult {
                name,
                content,
                is_error,
                ..
            } => {
                // Content may carry ANSI codes (kept for the TUI's colored
                // render); the transcript wants the plain text.
                let tag = if *is_error { "✗" } else { "✓" };
                (
                    format!("{tag} {name}: {}\n", ansi_plain(content)),
                    "",
                    false,
                )
            }
            Row::System { text, .. } => (format!("· {text}\n"), "", false),
        };
        let (header, body, mid_block) = block;
        if !out.is_empty() {
            out.push_str(if mid_block { "\n" } else { "\n\n" });
        }
        out.push_str(&header);
        out.push_str(body);
    }
    out.push_str("\n\n");
    out
}

/// Strip the synthetic fence opener `split_streaming_row` prepends to a tail
/// cut inside an open fence: a leading run of ≥3 identical marker chars
/// (` or ~) followed by a newline. Matched by run length — not a fixed
/// prefix — so any opener the splitter can write (3, 4, … markers) comes
/// back out whole; a shorter leading run, or markers with no newline after
/// them, is the model's own content and stays.
fn strip_synthetic_opener(t: &str) -> &str {
    let first = match t.chars().next() {
        Some(c @ ('`' | '~')) => c,
        _ => return t,
    };
    let run = t.chars().take_while(|&c| c == first).count();
    let bytes = run * first.len_utf8();
    if run >= 3 && t.as_bytes().get(bytes) == Some(&b'\n') {
        &t[bytes + 1..]
    } else {
        t
    }
}

/// Rebuild a resumed session's messages as display rows, so `--resume` opens
/// on a visible conversation instead of a blank transcript (the history lives
/// in the agent; the rows list starts empty). Display-only approximation:
/// ToolResult blocks name their tool via a tool_use_id → name lookup over all
/// ToolUse blocks (`elapsed` is None — the field exists for exactly this
/// case), image/PDF attachments become lightweight placeholder rows (the bytes
/// stay in the agent's context), and multi-block messages flatten in
/// order — text accumulated before a tool block flushes as its own row so the
/// interleaving reads like a live session.
pub(crate) fn transcript_rows(msgs: &[Message]) -> Vec<Row> {
    let mut names: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for m in msgs {
        for b in &m.content {
            if let Block::ToolUse { id, name, .. } = b {
                names.insert(id.as_str(), name.as_str());
            }
        }
    }
    let role_row = |role: Role, text: String| match role {
        Role::User => Row::User(text),
        Role::Assistant => Row::Assistant(text),
    };
    let mut rows = Vec::new();
    for m in msgs {
        let mut text = String::new();
        for b in &m.content {
            let flush = |rows: &mut Vec<Row>, text: &mut String| {
                if !text.is_empty() {
                    rows.push(role_row(m.role, std::mem::take(text)));
                }
            };
            match b {
                Block::Text(t) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
                Block::Image { .. } => {
                    flush(&mut rows, &mut text);
                    rows.push(role_row(m.role, "[image attached]".into()));
                }
                Block::Document { .. } => {
                    flush(&mut rows, &mut text);
                    rows.push(role_row(m.role, "[PDF document attached]".into()));
                }
                Block::ToolUse { name, input, .. } => {
                    flush(&mut rows, &mut text);
                    let input = strip_controls_json(input);
                    let collapsed = compact(&input);
                    rows.push(Row::ToolCall {
                        name: name.clone(),
                        input,
                        collapsed,
                        expanded: false,
                        started: Instant::now(),
                    });
                }
                Block::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } => {
                    flush(&mut rows, &mut text);
                    let name = names
                        .get(tool_use_id.as_str())
                        .copied()
                        .unwrap_or("(unknown)");
                    let collapsed = crate::agent::preview(&ansi_plain(content));
                    rows.push(Row::ToolResult {
                        name: name.to_string(),
                        content: content.clone(),
                        collapsed,
                        is_error: *is_error,
                        expanded: false,
                        // No call row started in THIS process — the duration
                        // of a pre-resume call is unknowable.
                        elapsed: None,
                    });
                }
                // non_exhaustive forward-compat: a future block type renders
                // nothing in the transcript until given an arm.
                _ => {}
            }
        }
        if !text.is_empty() {
            rows.push(role_row(m.role, text));
        }
    }
    rows
}

/// Render a role body with the icon inline on the first line: `❯ hello`
/// instead of a standalone `❯ You` header row. Continuation lines indent to
/// the same text column, so the layout keeps its alignment but every message
/// saves one screen row. An empty body still shows the bare icon.
fn push_role(
    lines: &mut Vec<Line<'static>>,
    icon: &str,
    icon_style: Style,
    text: &str,
    body_style: Style,
) {
    let mut body = text.lines();
    let first = Line::from(vec![
        Span::styled(icon.to_string(), icon_style),
        Span::styled(body.next().unwrap_or_default().to_string(), body_style),
    ]);
    lines.push(first);
    for l in body {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(ROLE_BODY_INDENT)),
            Span::styled(l.to_string(), body_style),
        ]));
    }
}

/// Indent rendered Markdown or ANSI lines without flattening their spans.
fn indent_lines(lines: &mut [Line<'static>], width: usize) {
    for line in lines {
        line.spans.insert(0, Span::raw(" ".repeat(width)));
    }
}

/// Tool heads are wrapped by the shared final layout pass. Its continuation
/// prefix keeps the four-column execution indent; pre-wrapping here used to
/// make the final pass lose the gutter or wrap the same text twice.
fn push_tool_head(lines: &mut Vec<Line<'static>>, head: Line<'static>) {
    lines.push(head);
}

/// Build styled history lines for the conversation. Used two ways: to render
/// finalized rows into the terminal scrollback (via `commit_rows`), and to
/// render the in-flight streaming row live in the viewport zone. Split out of
/// the draw/commit paths so role hierarchy, colors, `│ ` guides, and the
/// streaming cursor are unit-testable without a terminal.
///
/// Render one Assistant(-shaped) body: markdown via MdCache, indented under
/// the role column. `head` swaps the first line's indent for the bold `◆`
/// icon (message head); a continuation chunk keeps the plain indent so its
/// text aligns under the head's body. The live-streaming `▌` cursor lands on
/// this row's last line when `live` selects it AND it is the last row.
// (8 params: the row context the shared body render needs; a struct just for
// this would add noise — same allow as build_history_lines.)
#[allow(clippy::too_many_arguments)]
fn push_assistant_body(
    lines: &mut Vec<Line<'static>>,
    md_cache: &mut MdCache,
    inner_w: usize,
    t: &str,
    live: LiveRow,
    idx: usize,
    rows_len: usize,
    head: bool,
) {
    let markdown_width = inner_w.saturating_sub(ROLE_BODY_INDENT).max(1);
    let render = |txt: &str| -> Vec<Line<'static>> {
        render_markdown(txt, markdown_width)
            .iter()
            .map(line_to_owned)
            .collect()
    };
    // M6: the LIVE streaming row (live != None) re-renders on every redraw —
    // including busy clock ticks between deltas — so it goes through the
    // single-slot MdCache keyed by (text, width). The COMMIT path (live ==
    // None) renders each row exactly once and never re-reads the result, so
    // it renders directly WITHOUT touching the cache: a committed entry used
    // to be inserted and then never read again (pure dead weight), and each
    // insertion would also have evicted the streaming row's live slot.
    let mut rendered = if live == LiveRow::None {
        render(t)
    } else {
        md_cache.get_or_render(t, markdown_width, render).to_vec()
    };
    if rendered.is_empty() {
        rendered.push(Line::default());
    }
    indent_lines(&mut rendered, ROLE_BODY_INDENT);
    if head {
        // Inline role icon: the first body line's indent becomes the `◆`
        // marker — same column, one row saved, no header line.
        rendered[0].spans[0] = Span::styled(
            AGENT_MARK.to_string(),
            Style::default().fg(AGENT_FG).add_modifier(Modifier::BOLD),
        );
    }
    lines.extend(rendered);
    // Streaming cursor on the live assistant row.
    if live == LiveRow::Text && idx == rows_len - 1 {
        if let Some(last) = lines.last_mut() {
            last.spans
                .push(Span::styled("▌".to_string(), Style::default().fg(AGENT_FG)));
        }
    }
}

const THINKING_FOLD_ROWS: usize = 8;
const THINKING_PREVIEW_ROWS: usize = 3;

fn trailing_graphemes(text: &str, max: usize) -> &str {
    if max == 0 {
        return &text[text.len()..];
    }
    text.grapheme_indices(true)
        .rev()
        .nth(max - 1)
        .map_or(text, |(i, _)| &text[i..])
}

/// Tail used by the live Thinking viewport. Bound the source BEFORE wrapping:
/// scanning/re-wrapping the whole trace per delta would reintroduce quadratic
/// streaming cost on long reasoning blocks.
pub(super) fn live_thinking_tail(text: &str, width: usize) -> &str {
    let budget = width.saturating_mul(INLINE_HEIGHT as usize + 2).max(128);
    trailing_graphemes(text, budget)
}

/// Shared plain-prose Thinking renderer. Short traces stay inline; long FINAL
/// traces commit a fold header plus a short tail; explicit reprints and live
/// tails use the same gutters without Markdown parsing.
fn push_thinking(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    inner_w: usize,
    live: bool,
    expanded: bool,
) {
    if text.trim().is_empty() {
        return;
    }
    let label_style = Style::default()
        .fg(THINKING_FG)
        .add_modifier(Modifier::ITALIC);
    let body_w = inner_w.saturating_sub(ROLE_BODY_INDENT).max(1);
    // Real traces end with "\n" (often "\n\n"). Trailing blank rows are not
    // content: in a FINAL trace they count toward neither the fold decision,
    // the header's row count, nor the three-row preview — otherwise the
    // preview spends one of its rows on an empty line.
    let trimmed = if live { text } else { text.trim_end() };
    let total_rows = (!live).then(|| est_display_rows(trimmed, body_w));
    let folded = !expanded && total_rows.is_some_and(|n| n > THINKING_FOLD_ROWS);
    if folded || expanded {
        let label = if expanded { "full" } else { "folded" };
        let detail = total_rows
            .map(|n| format!(" · {n} rows"))
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled("✻ Thinking", label_style.add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" · {label}{detail} · /thinking"),
                Style::default().fg(dim_fg()),
            ),
        ]));
    }
    let body = if live {
        live_thinking_tail(text, body_w)
    } else if folded {
        let starts = display_row_starts(trimmed, body_w);
        let at = starts
            .len()
            .checked_sub(THINKING_PREVIEW_ROWS)
            .and_then(|i| starts.get(i))
            .copied()
            .unwrap_or(0);
        &trimmed[at..]
    } else {
        text
    };
    let mut rendered: Vec<Line<'static>> = body
        .split('\n')
        .flat_map(|l| {
            wrap_line(
                &Line::from(Span::styled(l.to_string(), label_style)),
                body_w,
            )
        })
        .collect();
    indent_lines(&mut rendered, ROLE_BODY_INDENT);
    if !folded && !expanded {
        rendered[0].spans[0] = Span::styled("✻ ".to_string(), label_style);
    }
    if live {
        if let Some(last) = rendered.last_mut() {
            last.spans.push(Span::styled("▌".to_string(), label_style));
        }
    }
    lines.extend(rendered);
}

/// `▌` cursor. `None` for the commit path (rows are final); `Text`/`Think`
/// for the single in-flight row rendered in the viewport's transcript zone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum LiveRow {
    None,
    Text,
    Think,
}

/// `inner_w` is the content width.
/// `live` appends a `▌` cursor to the in-flight row's last line — the lines
/// are clones out of MdCache, so the cached render stays clean.
/// `thinking` synthesizes one transient `◆…` tail line while the agent has
/// started a turn but produced no visible output yet. It is render-only: not
/// a row, so MdCache and the transcript are untouched.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_history_lines(
    rows: &[Row],
    md_cache: &mut MdCache,
    inner_w: usize,
    live: LiveRow,
    thinking: bool,
    trailing_spacer: bool,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let marker = TOOL_HEAD_MARK;
    for (idx, row) in rows.iter().enumerate() {
        match row {
            Row::User(t) => {
                push_role(
                    &mut lines,
                    "❯ ",
                    Style::default().fg(USER_FG).add_modifier(Modifier::BOLD),
                    t,
                    Style::default().add_modifier(Modifier::BOLD),
                );
            }
            Row::Assistant(t) => {
                push_assistant_body(
                    &mut lines,
                    md_cache,
                    inner_w,
                    t,
                    live,
                    idx,
                    rows.len(),
                    true,
                );
            }
            Row::AssistantCont { text: t, .. } => {
                // An incrementally-committed chunk: same body, no `◆` head —
                // one reply must not read as several messages.
                push_assistant_body(
                    &mut lines,
                    md_cache,
                    inner_w,
                    t,
                    live,
                    idx,
                    rows.len(),
                    false,
                );
            }
            Row::Thinking { text: t, .. } => {
                push_thinking(&mut lines, t, inner_w, live == LiveRow::Think, false);
            }
            Row::ThinkingFull(t) => {
                push_thinking(&mut lines, t, inner_w, false, true);
            }
            Row::ToolCall {
                name,
                input,
                collapsed,
                expanded,
                ..
            } => {
                let name_span = format!("{marker}→ {name}");
                let has_summary = !*expanded && !collapsed.is_empty();
                let head = Line::from(vec![
                    Span::styled(
                        name_span.clone(),
                        Style::default()
                            .fg(TOOL_CALL_FG)
                            .add_modifier(Modifier::BOLD),
                    ),
                    if has_summary {
                        Span::styled(
                            format!("  {}", collapsed.lines().next().unwrap_or_default()),
                            Style::default().fg(dim_fg()),
                        )
                    } else {
                        Span::raw(String::new())
                    },
                ]);
                push_tool_head(&mut lines, head);
                // Tool-call input is JSON sanitized at entry (ANSI inside it
                // is meaningless); ansi_guided_lines is a pass-through here
                // but keeps one uniform body path with ToolResult.
                if *expanded {
                    let body =
                        serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string());
                    let mut body = ansi_guided_lines(&body, Style::default().fg(dim_fg()));
                    indent_lines(&mut body, TOOL_INDENT);
                    lines.extend(body);
                }
            }
            Row::ToolResult {
                name,
                content,
                collapsed,
                is_error,
                expanded,
                elapsed,
            } => {
                let (tag, head_fg) = if *is_error {
                    ("✗", TOOL_ERR_FG)
                } else {
                    ("✓", TOOL_OK_FG)
                };
                // The ok head stays UNBOLD: a green bold "✓ shell" reads as a
                // second "◆" agent icon in a long execution flow; only the
                // failure glyph earns the emphasis.
                let head_style = if *is_error {
                    Style::default().fg(head_fg).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(head_fg)
                };
                let dur = elapsed
                    .as_deref()
                    .map(|e| format!(" · {e}"))
                    .unwrap_or_default();
                let name_span = format!("{marker}{tag} {name}{dur}");
                let has_summary = !*expanded && !collapsed.is_empty();
                let head = Line::from(vec![
                    Span::styled(name_span.clone(), head_style),
                    if has_summary {
                        let preview = ansi_plain(collapsed);
                        Span::styled(
                            format!("  {}", preview.lines().next().unwrap_or_default()),
                            Style::default().fg(if *is_error { TOOL_ERR_FG } else { dim_fg() }),
                        )
                    } else {
                        Span::raw(String::new())
                    },
                ]);
                push_tool_head(&mut lines, head);
                // Tool output keeps its ANSI colors: content arrives raw (the
                // parser strips non-SGR sequences and bare control chars, so
                // this path no longer needs strip_controls at entry).
                if *expanded {
                    let body_style = if *is_error {
                        Style::default().fg(TOOL_ERR_FG)
                    } else {
                        Style::default().fg(dim_fg())
                    };
                    let mut body = ansi_guided_lines(content, body_style);
                    indent_lines(&mut body, TOOL_INDENT);
                    lines.extend(body);
                }
            }
            Row::System { text, level } => {
                let fg = match level {
                    SysLevel::Info => dim_fg(),
                    SysLevel::Warn => TOOL_CALL_FG,
                    SysLevel::Error => TOOL_ERR_FG,
                };
                // One-line layout: the "· " marker rides the first text line
                // (a bare "·" dangling on its own line read as a bug);
                // continuation lines align under it. Info notices nest in the
                // tool gutter; warn/error rise to column 0 so they stand out
                // from the execution detail they interrupt.
                let (marker_pad, cont_pad) = match level {
                    SysLevel::Info => ("    · ", "      "),
                    SysLevel::Warn => ("! ", "  "),
                    SysLevel::Error => ("✗ ", "  "),
                };
                let marker_style = Style::default().fg(fg).add_modifier(Modifier::BOLD);
                let body_style = Style::default().fg(fg);
                let mut it = text.lines();
                lines.push(Line::from(vec![
                    Span::styled(marker_pad.to_string(), marker_style),
                    Span::styled(it.next().unwrap_or("").to_string(), body_style),
                ]));
                for l in it {
                    lines.push(Line::from(Span::styled(
                        format!("{cont_pad}{l}"),
                        body_style,
                    )));
                }
            }
        }
        // Row-boundary spacers. Base rule: a blank before the next user/system
        // row, or at the end of the history. On top:
        // adjacent assistant-shaped rows get a gap (an incrementally-split
        // chunk recovers the paragraph gap the split consumed; two separate
        // messages stop reading as one blob), and a Thinking block gets a
        // blank after it (trace vs answer separation) and before it unless it
        // directly follows the user's message (mirrors the tight
        // user→assistant look). Tight chunks are the exception: cut mid-way
        // through a paragraph/trace, they render flush against the row they
        // continue — a blank there would tear one block into two. Tool rows
        // stay tight against the flow they belong to.
        // `trailing_spacer`: whether the LAST row of this (possibly partial)
        // view ends the history. Incremental commits render SLICES of `rows`
        // — the slice's last row is usually followed by the still-streaming
        // tail, and an unconditional end-of-history blank there would insert
        // a spurious empty line into scrollback after every mid-stream batch
        // (tight chunks would read as torn paragraphs).
        let spacer = match rows.get(idx + 1) {
            None => trailing_spacer,
            Some(Row::User(_) | Row::System { .. }) => true,
            Some(Row::Thinking { tight: true, .. }) => false,
            Some(Row::Thinking { .. } | Row::ThinkingFull(_)) => !matches!(row, Row::User(_)),
            Some(Row::AssistantCont { tight: true, .. }) => false,
            Some(Row::Assistant(_) | Row::AssistantCont { .. }) => matches!(
                row,
                Row::Assistant(_)
                    | Row::AssistantCont { .. }
                    | Row::Thinking { .. }
                    | Row::ThinkingFull(_)
            ),
            Some(_) => false,
        };
        if spacer {
            lines.push(Line::default());
        }
    }
    // Transient thinking placeholder: the agent's icon plus an ellipsis while
    // a turn has started but nothing visible has streamed yet.
    if thinking {
        lines.push(Line::from(vec![
            Span::styled(
                AGENT_MARK,
                Style::default().fg(AGENT_FG).add_modifier(Modifier::BOLD),
            ),
            Span::styled("…", Style::default().fg(dim_fg())),
        ]));
    }
    lines
}

/// Wall-clock duration of a tool call, pre-formatted for the result header/// ("1.2s"). Matches the NEAREST same-name ToolCall row scanning backwards:
/// parallel sub-agents interleave their calls, so a name can repeat and
/// nearest-back is the best available approximation. None when no same-name
/// call row exists (e.g. a resumed session whose calls predate the row list).
pub(super) fn tool_elapsed_for(rows: &[Row], name: &str) -> Option<String> {
    rows.iter().rev().find_map(|r| match r {
        Row::ToolCall {
            name: n, started, ..
        } if n == name => Some(fmt_tool_elapsed(started.elapsed())),
        _ => None,
    })
}

/// Auto-expand contract for a tool result row: a FAILED call with SHORT
/// output (a command's stderr) opens so the user reads the error right away;
/// long dumps stay collapsed so they don't flood the scrollback, and
/// successful results never auto-open.
pub(super) fn auto_expand_result(is_error: bool, content: &str) -> bool {
    const MAX_AUTO_EXPAND_LINES: usize = 20;
    const MAX_AUTO_EXPAND_CHARS: usize = 2_000;
    is_error
        && content.lines().count() <= MAX_AUTO_EXPAND_LINES
        && content.chars().count() <= MAX_AUTO_EXPAND_CHARS
}

/// `/out [n]`: the nth-last ToolResult row (1-based, newest first), cloned
/// with `expanded: true` so the reprint opens the full output. Error results
/// participate too — a long failure that stayed collapsed can be opened the
/// same way. None when n is past the available results (or 0, which has no
/// nth-last meaning).
pub(super) fn nth_last_tool_result(rows: &[Row], n: usize) -> Option<Row> {
    let mut row = rows
        .iter()
        .rev()
        .filter(|r| matches!(r, Row::ToolResult { .. }))
        .nth(n.checked_sub(1)?)?
        .clone();
    if let Row::ToolResult { expanded, .. } = &mut row {
        *expanded = true;
    }
    Some(row)
}

/// `/call [n]`: clone the nth-last call and expand its full JSON input.
pub(super) fn nth_last_tool_call(rows: &[Row], n: usize) -> Option<Row> {
    let mut row = rows
        .iter()
        .rev()
        .filter(|r| matches!(r, Row::ToolCall { .. }))
        .nth(n.checked_sub(1)?)?
        .clone();
    if let Row::ToolCall { expanded, .. } = &mut row {
        *expanded = true;
    }
    Some(row)
}

/// `/thinking [n]`: reprint the complete original trace without mutating the
/// compact row that may already live in terminal scrollback.
pub(super) fn nth_last_thinking(rows: &[Row], n: usize) -> Option<Row> {
    rows.iter()
        .rev()
        .filter_map(|r| match r {
            Row::Thinking { text, .. } => Some(text),
            _ => None,
        })
        .nth(n.checked_sub(1)?)
        .map(|text| Row::ThinkingFull(text.clone()))
}

/// Ctrl+O: whichever detail-bearing row happened most recently wins.
pub(super) fn latest_detail(rows: &[Row]) -> Option<Row> {
    rows.iter().rev().find_map(|r| match r {
        Row::ToolResult { .. } => {
            let mut row = r.clone();
            if let Row::ToolResult { expanded, .. } = &mut row {
                *expanded = true;
            }
            Some(row)
        }
        Row::ToolCall { .. } => {
            let mut row = r.clone();
            if let Row::ToolCall { expanded, .. } = &mut row {
                *expanded = true;
            }
            Some(row)
        }
        Row::Thinking { text, .. } => Some(Row::ThinkingFull(text.clone())),
        _ => None,
    })
}

/// Render-only state for tool calls currently executing, mirroring the
/// `thinking` placeholder's contract: synthesized at render time, never
/// entering rows / MdCache / the transcript. The ToolCall row itself commits
/// to scrollback the moment it appears, so without this the live zone goes
/// blank for the whole execution — a 60s shell showed nothing but the
/// input-box spinner and the whole-run clock, with no per-tool timing.
pub(super) struct ToolRunning {
    /// (display name, start) per in-flight call; parallel `task` fan-out can
    /// legitimately hold several at once.
    pub(super) calls: Vec<(String, Instant)>,
}

impl ToolRunning {
    pub(super) fn new() -> Self {
        Self { calls: Vec::new() }
    }

    /// A tool call started (parent or sub-agent arm).
    pub(super) fn on_call(&mut self, name: &str) {
        self.calls.push((name.to_string(), Instant::now()));
    }

    /// A tool result arrived: remove the FIRST same-name entry. With no
    /// same-name entry (interleaved names make the matching approximate,
    /// like `tool_elapsed_for`) the oldest drops; an empty list is a no-op.
    pub(super) fn on_result(&mut self, name: &str) {
        if self.calls.is_empty() {
            return;
        }
        let pos = self.calls.iter().position(|(n, _)| n == name).unwrap_or(0);
        self.calls.remove(pos);
    }

    pub(super) fn clear(&mut self) {
        self.calls.clear();
    }
}

/// The live-zone placeholder line while tools execute: `→ name · 1.2s…`, dim
/// elapsed (read at render time so the busy clock tick keeps it moving), with
/// `(+N)` when more than one call is in flight. None when nothing runs.
pub(super) fn tool_running_line(state: &ToolRunning, now: Instant) -> Option<Line<'static>> {
    let (name, started) = state.calls.first()?;
    let elapsed = fmt_tool_elapsed(now.checked_duration_since(*started).unwrap_or_default());
    let more = state.calls.len().saturating_sub(1);
    let tail = if more > 0 {
        format!(" · {elapsed}… (+{more})")
    } else {
        format!(" · {elapsed}…")
    };
    Some(Line::from(vec![
        Span::styled(
            format!("{TOOL_HEAD_MARK}→ {name}"),
            Style::default()
                .fg(TOOL_CALL_FG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(tail, Style::default().fg(dim_fg())),
    ]))
}

/// Render finalized rows to text and commit them into the terminal scrollback
/// above the inline viewport via `insert_before`. The live region slides down
/// by exactly the rendered height; the host terminal's native
/// selection/scroll/search then own those lines.
/// How far the commit watermark may advance from `committed`. Pending queue
/// entries are deliberately NOT Rows: they live in the app's FIFO and a
/// render-only preview, so they cannot pin later tool results out of native
/// scrollback or invalidate the watermark when removed. The only held-back
/// row is a trailing in-flight Assistant/Thinking while its stream is open.
pub(super) fn commit_pending_end(rows: &[Row], committed: usize, stream_open: bool) -> usize {
    let mut pending_end = rows.len();
    // Hold back an in-flight trailing Assistant/AssistantCont/Thinking while
    // its stream is open. Whichever stream is open, its mutating row is the
    // trailing one (every other event either closes the stream or pushes a
    // later row), so one trailing check covers all kinds.
    if stream_open
        && pending_end > committed
        && matches!(
            rows[pending_end - 1],
            Row::Assistant(_) | Row::AssistantCont { .. } | Row::Thinking { .. }
        )
    {
        pending_end -= 1;
    }
    pending_end
}

/// Index of the row to render live in the viewport's transcript zone while a
/// stream is open, else None. This is the row `append_delta` /
/// `append_thinking_delta` is currently mutating — the LAST Assistant/Thinking
/// row in `rows`. It is deliberately independent of `commit_pending_end`:
/// finalized rows may already have left the live viewport while a later stream
/// is still open.
pub(super) fn inflight_row_index(rows: &[Row], stream_open: bool) -> Option<usize> {
    if stream_open {
        rows.iter().rposition(|r| {
            matches!(
                r,
                Row::Assistant(_) | Row::AssistantCont { .. } | Row::Thinking { .. }
            )
        })
    } else {
        None
    }
}

/// Append a streamed parent-agent text delta to the trailing Assistant row —
/// opening a NEW Assistant row when the tail is anything else (a System notice,
/// a tool row, or another stream's bubble pushed mid-stream). The old code
/// dropped the delta in that case, silently losing output. Control characters
/// are stripped here, once, at the row entry point.
pub(super) fn append_delta(rows: &mut Vec<Row>, streaming: &mut bool, text: &str) {
    if !*streaming
        || !matches!(
            rows.last(),
            Some(Row::Assistant(_) | Row::AssistantCont { .. })
        )
    {
        rows.push(Row::Assistant(String::new()));
        *streaming = true;
    }
    if let Some(Row::Assistant(buf) | Row::AssistantCont { text: buf, .. }) = rows.last_mut() {
        buf.push_str(&strip_controls(text));
    }
}

/// Byte offset where the tail of `text` may be split off: the START of the
/// last paragraph that follows a blank line OUTSIDE any open code fence
/// (``` or ~~~, tracked by the shared [`fence_step`] scan), so everything
/// before it is final (closed markdown blocks). None when no safe boundary
/// exists. The offset is where a new paragraph's first line begins; the
/// `\n\n` separator itself stays with the prefix and is trimmed there.
pub(super) fn last_safe_boundary(text: &str) -> Option<usize> {
    let mut best: Option<usize> = None;
    // Open fence (marker char × length) — tilde fences count exactly like
    // backtick ones (the old flip-on-``` toggle cut ~~~ blocks mid-content).
    let mut fence: Option<(char, usize)> = None;
    let mut prev_blank = false;
    let mut seen_content = false;
    let mut offset = 0usize;
    for line in text.split('\n') {
        let is_blank = line.trim().is_empty();
        // A fence line classified BEFORE the boundary check of the NEXT line:
        // cutting right before an opener is safe, cutting before the line
        // after a closer only becomes safe once the fence has closed.
        if prev_blank && !is_blank && fence.is_none() && seen_content {
            best = Some(offset);
        }
        fence = fence_next(line, fence);
        seen_content |= !is_blank;
        prev_blank = is_blank;
        offset += line.len() + 1; // +1 for the '\n' split_inclusive-style step
    }
    best
}

/// How many display rows of a split row stay live in the viewport once
/// incremental commits have started. The transcript zone shows only ~4 lines
/// anyway; keeping the tail at two means committed rows flow up into
/// scrollback as they are written (Claude Code's feel) instead of cycling
/// inside the box until the paragraph closes.
pub(super) const LIVE_TAIL_ROWS: usize = 2;

/// Whether `at` falls inside an unmatched inline-code backtick string. Fenced
/// blocks are skipped: their backticks are literal code content, and the fence
/// reopen machinery already handles those cuts. Partial source lines still
/// participate in inline parsing but cannot open/close a fenced block.
fn inline_code_open_at(text: &str, at: usize) -> bool {
    fn scan_inline(line: &str, open: &mut Option<usize>) {
        let bytes = line.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] != b'`' {
                i += 1;
                continue;
            }
            let start = i;
            while i < bytes.len() && bytes[i] == b'`' {
                i += 1;
            }
            let run = i - start;
            match *open {
                Some(want) if want == run => *open = None,
                Some(_) => {}
                None => *open = Some(run),
            }
        }
    }

    let mut fence = None;
    let mut inline = None;
    for segment in text[..at].split_inclusive('\n') {
        let complete = segment.ends_with('\n');
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        if inline.is_some() {
            scan_inline(line, &mut inline);
            continue;
        }
        if complete {
            let was_fenced = fence.is_some();
            fence = fence_next(line, fence);
            if was_fenced || fence.is_some() {
                continue;
            }
        } else if fence.is_some() {
            continue;
        }
        scan_inline(line, &mut inline);
    }
    inline.is_some()
}

/// Byte offset where a DISPLAY-row cut may split `text` so the tail keeps
/// ~`keep_rows` wrapped rows: the cut always sits on a wrap boundary (a char
/// that starts a fresh display row — the char after a `\n`, or the char that
/// overflows the width). Returns (offset, fence) — `fence` is
/// `Some((ch, len))` when the cut lands INSIDE an unclosed fence of marker
/// char `ch` whose opener was `len` markers long; the caller then re-opens
/// the fence in the tail with the same marker at that length (min 3) so both
/// halves render as code — a length-blind 3-marker reopen would let the
/// block's own shorter marker lines close the synthetic opener. A cut
/// landing exactly ON the fence's closing line slides past it (the closer
/// joins the prefix, tail starts fence-free). None while the text is too
/// short. Widths use the same grapheme-cluster + unicode-width tables as the
/// renderer, so CJK and joined emoji cannot be split across the seam.
pub(super) fn soft_cut_offset(
    text: &str,
    width: usize,
    keep_rows: usize,
) -> Option<(usize, Option<(char, usize)>)> {
    if keep_rows == 0 {
        return None;
    }
    let starts = display_row_starts(text, width);
    if starts.len() <= keep_rows {
        return None;
    }
    let at = starts[starts.len() - keep_rows];
    if at == 0 || at >= text.len() {
        return None;
    }
    // Unlike fenced blocks, an inline-code span cannot be reconstructed by a
    // synthetic opener in the tail. Wait for its closing backtick string
    // rather than committing two chunks with visible unmatched backticks.
    if inline_code_open_at(text, at) {
        return None;
    }
    let mut fence: Option<(char, usize)> = None;
    // Only COMPLETE logical lines may change fence state. When a display-row
    // cut lands midway through a source line, that partial line is not a
    // fence closer/opener even if its prefix happens to look like one.
    let complete_end = text[..at].rfind('\n').map(|n| n + 1).unwrap_or(0);
    for line in text[..complete_end].split('\n') {
        fence = fence_next(line, fence);
    }
    let tail_line = text[at..].lines().next().unwrap_or("");
    if matches!(fence_step(tail_line, fence), FenceLine::Close) {
        let nl = text[at..].find('\n')?;
        let after = at + nl + 1;
        return (after < text.len()).then_some((after, None));
    }
    Some((at, fence))
}

/// Incremental commit: once the in-flight text outgrows the inline viewport
/// (the live zone can only ever show its tail anyway), split the trailing
/// streaming row. The prefix is final and commits into scrollback on the
/// next pass while the user keeps watching fresh content stream in the live
/// zone — instead of the whole reply appearing in one flush when the turn
/// ends (Claude Code's model).
///
/// Cut selection (`width` is the markdown body width the renderer wraps at):
/// - **Paragraph boundary** (`last_safe_boundary`, blank line outside
///   fences) — preferred whenever the tail it keeps live fits the current
///   threshold (in display rows): committed chunks stay whole markdown
///   blocks, split is `tight: false` (blank line restored between chunks).
/// - **Display-row boundary** (`soft_cut_offset`, all but the last
///   `LIVE_TAIL_ROWS` wrapped rows) — the fallback when the current
///   paragraph is ITSELF taller than the threshold (or no boundary exists
///   yet): without it a long paragraph — including the common
///   one-huge-logical-line prose paragraph — would cycle inside the live
///   box until it closes (the pre-row-cut behavior). The chunk is
///   `tight: true` (mid-paragraph: renders flush, transcript joins with
///   `\n`). Cutting inside an open code fence prepends a synthetic opener
///   to the tail — the fence's OWN marker char at the opener's length (min
///   3), so a cut ``` block re-opens as ``` and a cut ```` block as ````:
///   a shorter synthetic would let the block's own shorter marker lines
///   close it and leak the rest as markdown. The unclosed prefix still
///   renders as a code block, and the tail renders as code on its own.
/// - Once a row has been split at all, the threshold drops from viewport
///   height to `LIVE_TAIL_ROWS` so every further row commits as written —
///   an 8-row re-arm window would cycle visibly between cuts. Paragraph
///   boundaries still win there whenever the new paragraph is young (≤2
///   rows), so paragraph structure keeps its blank-line chunks.
///
/// - Assistant tails split into `Assistant + AssistantCont` (loose) or
///   `AssistantCont{tight} + AssistantCont{tight}` (row-level): only the
///   message head keeps the `◆` icon. A row that is already a Cont splits
///   into two Conts (the head is far behind).
/// - Thinking tails stay WHOLE: their live renderer shows a bounded tail and
///   their final representation is a compact fold plus a `/thinking` reprint
///   entry point, so no early chunks can escape into immutable scrollback.
/// - Sub-agent bubbles are NOT split: `task` returns short summaries by
///   contract, and an unnamed Cont chunk interleaved between two same-named
///   agents' bubbles would be ambiguous.
pub(super) fn split_streaming_row(rows: &mut Vec<Row>, width: usize) {
    // Long Thinking stays whole until close so its FINAL compact fold can be
    // committed atomically. Its live renderer shows only a bounded tail.
    if matches!(rows.last(), Some(Row::Thinking { .. })) {
        return;
    }
    // Steady-state threshold once any split happened (see doc above).
    let already_split = matches!(rows.last(), Some(Row::AssistantCont { .. }));
    let (tail, tight) = {
        let text = match rows.last_mut() {
            Some(Row::Assistant(t)) | Some(Row::AssistantCont { text: t, .. }) => t,
            _ => return,
        };
        let threshold = if already_split {
            LIVE_TAIL_ROWS
        } else {
            INLINE_HEIGHT as usize
        };
        if est_display_rows(text, width) <= threshold {
            return;
        }
        let para = last_safe_boundary(text);
        // A paragraph cut is preferred when what it keeps live fits the
        // threshold (in display rows) — a paragraph that is ITSELF taller
        // would keep cycling inside the live box until it closes, so we
        // fall to the row cut instead (see doc above).
        let para_usable =
            matches!(para, Some(p) if est_display_rows(&text[p..], width) <= threshold);
        let (at, tight, fence_reopen) =
            match (para_usable, soft_cut_offset(text, width, LIVE_TAIL_ROWS)) {
                (true, _) => (para.expect("para_usable implies Some"), false, None),
                (false, Some((l, fo))) => (l, true, fo),
                // Unreachable (est > threshold >= LIVE_TAIL_ROWS guarantees a
                // row cut) — kept as a guard, not a silent no-op path.
                (false, None) => return,
            };
        // Validate the complete cut on immutable slices first. Abandoning a
        // split must leave the streaming source byte-for-byte unchanged.
        if at >= text.len() {
            return;
        }
        let prefix_end = text[..at].trim_end_matches('\n').len();
        if text[..prefix_end].trim().is_empty() {
            return;
        }
        let mut tail = text.split_off(at);
        // Paragraph separators belong to neither rendered chunk; row cuts
        // normally have no trailing newline, so this is a no-op for them.
        text.truncate(prefix_end);
        // The cut landed inside an open fence (`fence_reopen` carries its
        // marker char and the opener's length): re-open the tail with the
        // fence's OWN marker at that length (min 3 — the shortest fence the
        // renderer accepts). Length-true matters: a 4-backtick block's own
        // ``` content lines must not close the synthetic ``` opener. A cut
        // ~~~ block re-opens as ~~~, not ```.
        if let Some((ch, len)) = fence_reopen {
            tail = format!("{}\n{tail}", ch.to_string().repeat(len.max(3)));
        }
        (tail, tight)
    };
    rows.push(Row::AssistantCont { text: tail, tight });
}

/// Append a ThinkingDelta to the trace row. Consecutive deltas merge while
/// the same stream is open (`thinking_stream` holds the tag: "" for the
/// parent, the sub-agent name for a child); any other row in between — the
/// answer starting, a tool call, another agent's bubble — naturally starts
/// a fresh ✻ block on re-entry. A sub-agent's row opens with `[name] ` in
/// the text so the render shows whose trace it is.
pub(super) fn append_thinking_delta(
    rows: &mut Vec<Row>,
    thinking_stream: &mut Option<String>,
    tag: &str,
    text: &str,
) {
    let merge = thinking_stream.as_deref() == Some(tag)
        && matches!(rows.last(), Some(Row::Thinking { .. }));
    if !merge {
        let head = if tag.is_empty() {
            String::new()
        } else {
            format!("[{tag}] ")
        };
        rows.push(Row::Thinking {
            text: head,
            tight: false,
        });
        *thinking_stream = Some(tag.to_string());
    }
    if let Some(Row::Thinking { text: buf, .. }) = rows.last_mut() {
        buf.push_str(&strip_controls(text));
    }
}

/// Sub-agent counterpart of append_delta: consecutive deltas from the same
/// named sub-agent merge into one `[{name}] `-prefixed bubble; a different
/// sub-agent — or a non-Assistant tail such as a notice or tool row — starts a
/// fresh bubble instead of dropping the delta (the old silent-drop bug).
pub(super) fn append_sub_delta(
    rows: &mut Vec<Row>,
    sub_streaming_name: &mut Option<String>,
    name: &str,
    text: &str,
) {
    let merge = sub_streaming_name.as_deref() == Some(name)
        && matches!(rows.last(), Some(Row::Assistant(_)));
    if !merge {
        rows.push(Row::Assistant(format!("[{name}] ")));
        *sub_streaming_name = Some(name.to_string());
    }
    if let Some(Row::Assistant(buf)) = rows.last_mut() {
        buf.push_str(&strip_controls(text));
    }
}

pub(super) fn compact(v: &serde_json::Value) -> String {
    crate::agent::preview(&v.to_string())
}
