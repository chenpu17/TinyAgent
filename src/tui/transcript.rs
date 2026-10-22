//! Conversation rows: the `Row` model and everything that builds or mutates
//! it — history line rendering, streamed-delta appenders, the incremental
//! streaming split, the `/export` transcript dump, and the resume-transcript
//! rebuild. Pure row/text logic; terminal painting lives in `render`.

use super::app::INLINE_HEIGHT;
use super::markdown::{
    ansi_guided_lines, ansi_plain, fence_next, fence_step, line_to_owned, render_markdown,
    FenceLine, MdCache,
};
use super::render::{est_display_rows, strip_controls, strip_controls_json, wrap_line};
use super::theme::{
    dim_fg, fmt_tool_elapsed, AGENT_FG, TOOL_CALL_FG, TOOL_ERR_FG, TOOL_OK_FG, USER_FG,
};
use crate::provider::{Block, Message, Role};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

/// Body indent beneath the standalone conversation role headers.
pub(super) const ROLE_BODY_INDENT: usize = 2;
const TOOL_INDENT: usize = 4;

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
    /// A user message queued behind an in-flight run: rendered as a dim
    /// `❯ … · queued` ghost. Removed (and replaced by the real Row::User)
    /// when the queue flush actually sends it; dropped if the queue is.
    Queued(String),
    Assistant(String),
    /// Continuation of the previous Assistant message — a chunk split off
    /// the streaming row by `split_streaming_row` once it outgrew the live
    /// viewport (incremental commit). Renders like Assistant but WITHOUT
    /// the `● ` icon (the message head already has it) — one reply must not
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
    /// (whitespace-only) render nothing. `tight: true` marks a mid-block
    /// chunk (line-level cut of a long trace): no `✻ ` marker, flush render.
    Thinking {
        text: String,
        tight: bool,
    },
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
/// synthetic opener — three of the fence's marker char, ``` or ~~~ (see
/// `split_streaming_row`) — so it renders as code on its own; stripped here,
/// the model never wrote that line. Corner case: a model-emitted bare fence
/// opener landing exactly on a cut line loses that line in the dump
/// (readability export, not an archive).
pub(super) fn render_transcript(rows: &[Row]) -> String {
    let mut out = String::new();
    for row in rows {
        // (block header, body, mid-block?) — a continuation chunk carries no
        // header of its own (the head chunk has it).
        let block: (String, &str, bool) = match row {
            Row::User(t) => ("You\n".into(), t.as_str(), false),
            Row::Queued(t) => ("You (queued)\n".into(), t.as_str(), false),
            Row::Assistant(t) if t.is_empty() => continue,
            Row::Assistant(t) => ("Tiny\n".into(), t.as_str(), false),
            Row::AssistantCont { text: t, .. } if t.is_empty() => continue,
            Row::AssistantCont { text: t, tight } => {
                let body = if *tight {
                    // Either marker shape (see split_streaming_row).
                    t.strip_prefix("```\n")
                        .or_else(|| t.strip_prefix("~~~\n"))
                        .unwrap_or(t)
                } else {
                    t.as_str()
                };
                (String::new(), body, *tight)
            }
            Row::Thinking { text: t, .. } if t.trim().is_empty() => continue,
            Row::Thinking { text: t, tight } => ("✻ Thinking\n".into(), t.as_str(), *tight),
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

/// Rebuild a resumed session's messages as display rows, so `--resume` opens
/// on a visible conversation instead of a blank transcript (the history lives
/// in the agent; the rows list starts empty). Display-only approximation:
/// ToolResult blocks name their tool via a tool_use_id → name lookup over all
/// ToolUse blocks (`elapsed` is None — the field exists for exactly this
/// case), images are skipped (the bytes stay in the agent's context; a
/// transcript line carries nothing), and multi-block messages flatten in
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
                Block::Image { .. } => {}    // display layer skips images
                Block::Document { .. } => {} // …and PDF documents likewise
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
/// saves one screen row. `suffix` (e.g. the queued ghost's ` · queued`) rides
/// on the first line. An empty body still shows the bare icon.
fn push_role(
    lines: &mut Vec<Line<'static>>,
    icon: &str,
    icon_style: Style,
    text: &str,
    body_style: Style,
    suffix: Option<(&str, Style)>,
) {
    let mut body = text.lines();
    let first = Line::from(vec![
        Span::styled(icon.to_string(), icon_style),
        Span::styled(body.next().unwrap_or_default().to_string(), body_style),
    ]);
    let mut first = first;
    if let Some((s, style)) = suffix {
        first.spans.push(Span::styled(s.to_string(), style));
    }
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

/// Push a tool head line. An over-long head (a wide collapsed summary on a
/// narrow terminal) wraps with a HANGING indent: continuation rows align
/// under the summary column (`hang`) instead of falling back to column 0,
/// which would break the tool gutter. Selection styling is the caller's
/// post-pass over the row's lines, not this function's concern.
fn push_tool_head(lines: &mut Vec<Line<'static>>, head: Line<'static>, hang: usize, width: usize) {
    // wrap_line rebuilds Lines with a default line-level style; carry the
    // head's.
    let line_style = head.style;
    let mut rows = wrap_line(&head, width);
    // wrap_line always yields at least one row.
    let mut wrapped: Vec<Line<'static>> = vec![rows.remove(0)];
    for row in rows {
        // Continuation rows carry the indent, so they re-wrap narrower.
        for mut cont in wrap_line(&row, width.saturating_sub(hang).max(1)) {
            cont.spans.insert(0, Span::raw(" ".repeat(hang)));
            wrapped.push(cont);
        }
    }
    for row in wrapped.iter_mut() {
        row.style = line_style;
    }
    lines.extend(wrapped);
}

/// Build styled history lines for the conversation. Used two ways: to render
/// finalized rows into the terminal scrollback (via `commit_rows`), and to
/// render the in-flight streaming row live in the viewport zone. Split out of
/// the draw/commit paths so role hierarchy, colors, `│ ` guides, the streaming
/// cursor, and the empty placeholder are unit-testable without a terminal.
///
/// Render one Assistant(-shaped) body: markdown via MdCache, indented under
/// the role column. `head` swaps the first line's indent for the bold `● `
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
        // Inline role icon: the first body line's indent becomes the `● `
        // marker — same column, one row saved, no header line.
        rendered[0].spans[0] = Span::styled(
            "● ".to_string(),
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

/// Which trailing row is live (still growing) in this render — drives the
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
/// `thinking` synthesizes one transient `● …` tail line while the agent has
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
    if rows.is_empty() && !thinking {
        // Empty conversation (post-/clear): the guide line plus a key
        // cheat-sheet — cheapest onboarding real estate there is. The newline
        // and cancel keys are the two highest-frequency confusions (a bare
        // Enter sends, so multi-line needs a discoverable escape hatch; Esc
        // only cancels mid-run), so they earn a line over everything else.
        let style = Style::default().fg(dim_fg()).add_modifier(Modifier::ITALIC);
        for l in [
            "  no messages yet — type below; /help lists commands",
            "  / completes commands · Enter sends",
            "  Alt+Enter (or \\+Enter) newline · Esc cancels a run",
            "  scroll / select / copy are native to your terminal",
        ] {
            lines.push(Line::from(Span::styled(l.to_string(), style)));
        }
        return lines;
    }
    let marker = "    ";
    for (idx, row) in rows.iter().enumerate() {
        match row {
            Row::User(t) => {
                push_role(
                    &mut lines,
                    "❯ ",
                    Style::default().fg(USER_FG).add_modifier(Modifier::BOLD),
                    t,
                    Style::default(),
                    None,
                );
            }
            Row::Queued(t) => {
                // Ghost for a message waiting behind the in-flight run: same
                // user shape, fully dimmed, with a `· queued` tag on the
                // first line so it can't be mistaken for a sent message.
                push_role(
                    &mut lines,
                    "❯ ",
                    Style::default().fg(dim_fg()).add_modifier(Modifier::BOLD),
                    t,
                    Style::default().fg(dim_fg()),
                    Some((" · queued", Style::default().fg(dim_fg()))),
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
                // An incrementally-committed chunk: same body, no `● ` head —
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
            Row::Thinking { text: t, tight } => {
                // Reasoning trace: plain dim-italic prose — deliberately NOT
                // markdown/MdCache so it can never read like the answer.
                // Wrapped here because wrap_history's final pass can't
                // re-indent continuation lines under the marker.
                if t.trim().is_empty() {
                    continue; // whitespace-only trace: render nothing
                }
                let style = Style::default().fg(dim_fg()).add_modifier(Modifier::ITALIC);
                let body_w = inner_w.saturating_sub(ROLE_BODY_INDENT).max(1);
                let mut rendered: Vec<Line<'static>> = t
                    .split('\n')
                    .flat_map(|l| {
                        wrap_line(&Line::from(Span::styled(l.to_string(), style)), body_w)
                    })
                    .collect();
                indent_lines(&mut rendered, ROLE_BODY_INDENT);
                // Inline marker over the first line's indent (same trick as
                // the role icons): `✻ ` announces a trace block in one row,
                // no header line spent. A tight (mid-block) chunk is a
                // continuation — it keeps the indent and reads as part of
                // the block above it.
                if !*tight {
                    rendered[0].spans[0] = Span::styled("✻ ".to_string(), style);
                }
                lines.extend(rendered);
                // Streaming cursor on the live trace row.
                if live == LiveRow::Think && idx == rows.len() - 1 {
                    if let Some(last) = lines.last_mut() {
                        last.spans.push(Span::styled("▌".to_string(), style));
                    }
                }
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
                // Continuation rows align under the summary (or the name when
                // bare); selection styling is the post-pass below.
                let hang = name_span.width() + if has_summary { 2 } else { 0 };
                push_tool_head(&mut lines, head, hang, inner_w);
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
                // second "● " agent icon in a long execution flow; only the
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
                let hang = name_span.width() + if has_summary { 2 } else { 0 };
                push_tool_head(&mut lines, head, hang, inner_w);
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
                    _ => ("· ", "  "),
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
        // Row-boundary spacers. Base rule (today's): a blank before the next
        // user / queued / system row, or at the end of the history. On top:
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
            Some(Row::User(_) | Row::Queued(_) | Row::System { .. }) => true,
            Some(Row::Thinking { tight: true, .. }) => false,
            Some(Row::Thinking { .. }) => !matches!(row, Row::User(_)),
            Some(Row::AssistantCont { tight: true, .. }) => false,
            Some(Row::Assistant(_) | Row::AssistantCont { .. }) => matches!(
                row,
                Row::Assistant(_) | Row::AssistantCont { .. } | Row::Thinking { .. }
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
                "● ",
                Style::default().fg(AGENT_FG).add_modifier(Modifier::BOLD),
            ),
            Span::styled("…", Style::default().fg(dim_fg())),
        ]));
    }
    lines
}

/// Remove the oldest ghost Row::Queued (FIFO, matching `queued.pop_front()`).
/// Returns true when a row was removed — row indices then shifted, so the
/// caller also drops the MdCache slot (content-keyed, so this is eager
/// hygiene rather than an anti-aliasing necessity).
pub(super) fn remove_oldest_queued(rows: &mut Vec<Row>) -> bool {
    let pos = rows.iter().position(|r| matches!(r, Row::Queued(_)));
    if let Some(i) = pos {
        rows.remove(i);
    }
    pos.is_some()
}

/// Remove every ghost Row::Queued (the whole queue was dropped). Returns true
/// when any were removed, for the same MdCache-hygiene reason as above.
pub(super) fn drop_queued_ghosts(rows: &mut Vec<Row>) -> bool {
    let had = rows.iter().any(|r| matches!(r, Row::Queued(_)));
    rows.retain(|r| !matches!(r, Row::Queued(_)));
    had
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
    is_error && content.lines().count() <= 20
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
            format!("→ {name}"),
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
/// How far the commit watermark may advance from `committed`. Two things are
/// held back, never written into the terminal's permanent scrollback:
///
/// 1. The **first `Row::Queued` ghost**, and therefore everything after it. A
///    queued ghost is a placeholder for a not-yet-sent user message; while it
///    waits, the in-flight run keeps appending real rows after it (tool calls,
///    tool results, warnings, compress notices, sub-agent rows). The old logic
///    only stripped *trailing* `Queued` rows, so the moment any real row landed
///    after a ghost the ghost was no longer trailing and got committed —
///    permanently leaving a dim placeholder in scrollback, AND breaking the
///    watermark's own invariant ("only uncommitted ghosts are ever removed"):
///    `flush_next_queued` would later `remove` that already-committed row,
///    shifting indices under `committed` and silently dropping the next real
///    row. Capping at the *first* ghost fixes both: the ghost and every later
///    row stay out of scrollback until the queue flushes (flush removes only
///    this still-uncommitted ghost, so the index invariant stays sound).
/// 2. While a stream is open, a trailing **`Row::Assistant`/`Row::Thinking`**
///    — the in-flight reply or reasoning trace is still mutating and is shown
///    live in the viewport, not committed until the stream closes.
pub(super) fn commit_pending_end(rows: &[Row], committed: usize, stream_open: bool) -> usize {
    // Cap at the first queued ghost (holds the ghost + everything after back).
    let mut pending_end = match rows[committed..]
        .iter()
        .position(|r| matches!(r, Row::Queued(_)))
    {
        Some(i) => committed + i,
        None => rows.len(),
    };
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
/// `append_thinking_delta` is currently mutating — the LAST
/// `Row::Assistant`/`Row::Thinking` in `rows` — which a queued ghost may
/// sit behind. It is deliberately INDEPENDENT of `commit_pending_end`: that
/// watermark caps at the first queued ghost, so when a new turn streams AFTER
/// a ghost, the watermark points at the stale pre-ghost Assistant, not the
/// live one. Deriving the in-flight index from the watermark (the old code)
/// rendered that stale row and hid the real streaming output.
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
/// opening a NEW Assistant row when the tail is anything else (a ghost
/// Row::Queued, a System notice, a tool row pushed mid-stream). The old code
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

/// Byte offset where a DISPLAY-row cut may split `text` so the tail keeps
/// ~`keep_rows` wrapped rows: the cut always sits on a wrap boundary (a char
/// that starts a fresh display row — the char after a `\n`, or the char that
/// overflows the width). Returns (offset, fence) — `fence` is `Some(ch)` when
/// the cut lands INSIDE an unclosed fence of marker char `ch`; the caller
/// then re-opens the fence (with its OWN marker char) in the tail so both
/// halves render as code. A cut landing exactly ON the fence's closing line
/// slides past it (the closer joins the prefix, tail starts fence-free).
/// None while the text is too short. Widths use unicode-width per char, the
/// same tables the renderer wraps with — drift only raggeds the seam
/// cosmetically, no text is lost or duplicated.
pub(super) fn soft_cut_offset(
    text: &str,
    width: usize,
    keep_rows: usize,
) -> Option<(usize, Option<char>)> {
    let w = width.max(1);
    if keep_rows == 0 {
        return None;
    }
    let total = est_display_rows(text, w);
    if total <= keep_rows {
        return None;
    }
    let target = total - keep_rows; // rows that stay with the prefix side
    let mut rows = 0usize; // display rows completed on the prefix side
    let mut fence: Option<(char, usize)> = None;
    let mut line_start = 0usize;
    let mut col = 0usize;
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            fence = fence_next(&text[line_start..i], fence);
            rows += 1; // a line always completes exactly one final row
            col = 0;
            line_start = i + 1;
            if rows == target {
                let at = i + 1;
                // The tail would start on the fence's closing line — slide
                // past it (may leave an empty tail when it was the last
                // line; the caller's empty-tail guard aborts the split).
                // fence_step can only say Close for the line that closes
                // the currently open fence, tilde or backtick alike.
                let tail_line = text[at..].lines().next().unwrap_or("");
                if matches!(fence_step(tail_line, fence), FenceLine::Close) {
                    let nl = text[at..].find('\n')?;
                    return Some((at + nl + 1, None));
                }
                return Some((at, fence.map(|(ch, _)| ch)));
            }
            continue;
        }
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if col + cw > w {
            rows += 1;
            col = 0;
            if rows == target {
                return Some((i, fence.map(|(ch, _)| ch)));
            }
        }
        col += cw;
    }
    None
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
///   to the tail — three of the fence's OWN marker char, so a cut ``` block
///   re-opens as ``` and a cut ~~~ block as ~~~: the unclosed prefix still
///   renders as a code block, and the tail renders as code on its own;
///   thinking rows skip this (plain prose render, a literal fence would
///   show).
/// - Once a row has been split at all, the threshold drops from viewport
///   height to `LIVE_TAIL_ROWS` so every further row commits as written —
///   an 8-row re-arm window would cycle visibly between cuts. Paragraph
///   boundaries still win there whenever the new paragraph is young (≤2
///   rows), so paragraph structure keeps its blank-line chunks.
///
/// - Assistant tails split into `Assistant + AssistantCont` (loose) or
///   `AssistantCont{tight} + AssistantCont{tight}` (row-level): only the
///   message head keeps the `● ` icon. A row that is already a Cont splits
///   into two Conts (the head is far behind).
/// - Thinking tails split into `Thinking + Thinking` (each non-tight chunk
///   carries its own `✻ ` marker, all-dim anyway, reads as trace sections;
///   tight chunks carry none and read as continuation).
/// - Sub-agent bubbles are NOT split: `task` returns short summaries by
///   contract, and an unnamed Cont chunk interleaved between two same-named
///   agents' bubbles would be ambiguous.
pub(super) fn split_streaming_row(rows: &mut Vec<Row>, width: usize) {
    let is_thinking = matches!(rows.last(), Some(Row::Thinking { .. }));
    // Steady-state threshold once any split happened (see doc above).
    let already_split = matches!(
        rows.last(),
        Some(Row::AssistantCont { .. }) | Some(Row::Thinking { tight: true, .. })
    );
    let (tail, tight) = {
        let text = match rows.last_mut() {
            Some(Row::Assistant(t))
            | Some(Row::AssistantCont { text: t, .. })
            | Some(Row::Thinking { text: t, .. }) => t,
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
        let mut tail = text.split_off(at);
        // Consume the separator newlines that stayed with the prefix
        // (paragraph cuts keep the `\n\n`; row cuts end at a char boundary).
        while text.ends_with('\n') {
            text.pop();
        }
        // An all-blank prefix would render as an empty message — undo.
        if text.trim().is_empty() {
            text.push_str(&tail);
            return;
        }
        if tail.is_empty() {
            return;
        }
        // The cut landed inside an open fence (`fence_reopen` carries its
        // marker char): re-open the tail with the fence's OWN marker — a
        // cut ~~~ block re-opens as ~~~, not ```. Thinking rows skip this
        // (plain prose render, a literal fence would show).
        if !is_thinking {
            if let Some(ch) = fence_reopen {
                tail = format!("{ch}{ch}{ch}\n{tail}");
            }
        }
        (tail, tight)
    };
    let row = if is_thinking {
        Row::Thinking { text: tail, tight }
    } else {
        Row::AssistantCont { text: tail, tight }
    };
    rows.push(row);
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
/// sub-agent — or a tail row that isn't an Assistant bubble (a notice or
/// ghost pushed mid-stream) — starts a fresh bubble instead of dropping the
/// delta (the old silent-drop bug).
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
