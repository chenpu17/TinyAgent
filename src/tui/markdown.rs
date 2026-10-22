//! Markdown and ANSI rendering: `render_markdown` (block-level fixes on top
//! of tui-markdown), the ANSI escape parser for tool output, the fence
//! scanner shared with the streaming split, and the single-slot `MdCache`.

use super::theme::{code_fg, dim_fg, MD_DEEP_FG, MD_H1_FG, MD_H23_FG};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// The standard ANSI palette (SGR 30-37 / 40-47) and the bright variant
/// (90-97 / 100-107), in SGR order.
const ANSI_8: [Color; 8] = [
    Color::Black,
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::Gray,
];
const ANSI_BRIGHT: [Color; 8] = [
    Color::DarkGray,
    Color::LightRed,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
    Color::White,
];

/// Apply one SGR parameter list ("1;31") to `style`. Supports reset, bold,
/// dim, italic, underline, default fg/bg (39/49) and the two 8-color ranges;
/// 256-color and RGB forms (38;5;n / 38;2;r;g;b and the 48* bg twins) have
/// their extra parameters consumed and reset that channel — carrying a full
/// palette here isn't worth it. Unknown parameters are ignored. An empty
/// parameter list is reset (ESC[m).
fn apply_sgr(style: &mut Style, params: &str) {
    let mut it = params.split(';');
    while let Some(p) = it.next() {
        let n: u16 = p.parse().unwrap_or(0); // "" (ESC[m) behaves as 0
        match n {
            0 => *style = Style::default(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            2 => *style = style.add_modifier(Modifier::DIM),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => *style = style.add_modifier(Modifier::UNDERLINED),
            30..=37 => style.fg = Some(ANSI_8[(n - 30) as usize]),
            40..=47 => style.bg = Some(ANSI_8[(n - 40) as usize]),
            90..=97 => style.fg = Some(ANSI_BRIGHT[(n - 90) as usize]),
            100..=107 => style.bg = Some(ANSI_BRIGHT[(n - 100) as usize]),
            39 => style.fg = None,
            49 => style.bg = None,
            38 | 48 => {
                match it.next().and_then(|m| m.parse::<u16>().ok()) {
                    Some(5) => {
                        it.next(); // palette index
                    }
                    Some(2) => {
                        it.next(); // r
                        it.next(); // g
                        it.next(); // b
                    }
                    _ => {}
                }
                if n == 38 {
                    style.fg = None;
                } else {
                    style.bg = None;
                }
            }
            _ => {}
        }
    }
}

/// Second bytes that legitimately introduce a two-char escape sequence
/// after a bare ESC: the charset-designation/intermediate range 0x20–0x2f
/// (`(`, `)`, `#`, `%`, …) plus the standalone Fe finals terminals actually
/// emit (RIS `c`, IND `D`, NEL `E`, RI `M`, DECSC `7`, DECRC `8`, DECKPAM
/// `=`, DECKPNM `>`). '\n', '\r' and ordinary letters are NOT introducers —
/// see the ESC arm in ansi_to_lines.
fn is_two_char_esc_introducer(c: char) -> bool {
    matches!(
        c,
        '\x20'..='\x2f' | '7' | '8' | '=' | '>' | 'c' | 'D' | 'E' | 'M'
    )
}

/// Parse tool-output text with ANSI escape sequences into styled lines.
/// SGR color/style sequences become span styles; every other escape sequence
/// (non-`m` CSI, OSC hyperlinks/titles, two-char ESC sequences) and every
/// remaining control char except \n and \t is stripped — so this REPLACES
/// strip_controls on the tool-output path, with color support added. A
/// sequence truncated at end-of-input (e.g. a collapsed preview cut mid-CSI)
/// is dropped without panicking.
pub(super) fn ansi_to_lines(text: &str) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut style = Style::default();
    // The input's last visible byte is a newline → the final line that
    // newline opened must be materialized by the tail flush below, or the
    // join in ansi_plain drops the trailing blank line the user saw on
    // screen (/export would end one line short of the display). No reset
    // is needed: the flag is only consulted when cur AND spans are empty,
    // and any visible char after the last '\n' leaves cur/spans non-empty
    // (flushing anyway).
    let mut ended_with_newline = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    // CSI: parameter/intermediate bytes until a final byte
                    // (@..=~). Only SGR ('m') is interpreted; the rest is
                    // stripped. End-of-input before a final byte → dropped.
                    let mut params = String::new();
                    for c2 in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c2) {
                            if c2 == 'm' {
                                // Style boundary: flush the text the PREVIOUS
                                // style covered before applying the new one,
                                // or a mid-line SGR change (red diff line
                                // reset to default) would restyle its head.
                                if !cur.is_empty() {
                                    spans.push(Span::styled(std::mem::take(&mut cur), style));
                                }
                                apply_sgr(&mut style, &params);
                            }
                            break;
                        }
                        params.push(c2);
                    }
                }
                Some(']') => {
                    chars.next();
                    // OSC: runs until BEL or ESC\. Stripped wholesale.
                    let mut prev_esc = false;
                    for c2 in chars.by_ref() {
                        if c2 == '\x07' || (prev_esc && c2 == '\\') {
                            break;
                        }
                        prev_esc = c2 == '\x1b';
                    }
                }
                // Two-character ESC sequences (ESC c, ESC ( B, …): strip the
                // ESC and the introducer, letting any printable tail through
                // the normal path. The introducer is consumed ONLY when it is
                // one this parser recognizes — a bare ESC next to ordinary
                // text must not swallow its neighbor: "a\x1b\nb" keeps the
                // newline, "\x1bXb" keeps the X. The ESC itself is dropped
                // either way.
                _ => {
                    if chars
                        .peek()
                        .is_some_and(|&c2| is_two_char_esc_introducer(c2))
                    {
                        chars.next();
                    }
                }
            },
            '\n' => {
                spans.push(Span::styled(std::mem::take(&mut cur), style));
                lines.push(Line::from(std::mem::take(&mut spans)));
                ended_with_newline = true;
            }
            // Expand tabs to 2 spaces: unicode-width counts \t as ZERO
            // columns while terminals render it 1–8, so a tabbed line wraps
            // late and hanging indents drift. ansi_plain shares this path, so
            // /export shows the same spacing the user saw on screen.
            '\t' => cur.push_str("  "),
            c if c.is_control() => {} // strip (same contract as strip_controls)
            c => cur.push(c),
        }
    }
    if !cur.is_empty() || !spans.is_empty() || ended_with_newline {
        spans.push(Span::styled(cur, style));
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

/// Tool-output body lines with the dim `│ ` guide: ANSI-styled spans keep
/// their colors; spans the source left unstyled (or reset to default) take
/// `base_style` (dim for ok results, red for errors).
pub(super) fn ansi_guided_lines(text: &str, base_style: Style) -> Vec<Line<'static>> {
    ansi_to_lines(text)
        .into_iter()
        .map(|l| {
            let mut spans = vec![Span::styled(
                "│ ".to_string(),
                Style::default().fg(dim_fg()),
            )];
            spans.extend(l.spans.into_iter().map(|s| {
                if s.style == Style::default() {
                    Span::styled(s.content.into_owned(), base_style)
                } else {
                    s
                }
            }));
            Line::from(spans)
        })
        .collect()
}

/// Plain-text view of ANSI-bearing tool output, for /export: styling and
/// escape sequences dropped, the visible text kept.
pub(super) fn ansi_plain(text: &str) -> String {
    ansi_to_lines(text)
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// True when a markdown line is a horizontal rule: at least 3 chars, all of
/// them `-` / `*` / `_` (whitespace ignored). tui-markdown emits a thematic
/// break as a literal "---" line, so this catches it post-parse.
pub(super) fn is_hrule(text: &str) -> bool {
    let n = text.chars().filter(|c| !c.is_whitespace()).count();
    n >= 3
        && text
            .chars()
            .all(|c| c.is_whitespace() || matches!(c, '-' | '*' | '_'))
}

/// Render a markdown string into styled lines. `tui-markdown` 0.3 styles inline
/// elements (bold / code) correctly but leaves block elements literal: an ATX
/// header renders as plain "## Title" (markers visible, no emphasis), a list
/// item as "- text", a fenced code block keeps its ``` fence lines with the
/// code unstyled, and a thematic break comes through as a literal "---" line.
/// We keep its inline work and fix only the block lines — headers become a
/// bold, level-colored title with the `#` markers stripped, `-`/`*`/`+`
/// bullets become `•`, the ``` fence lines are dropped and the enclosed code
/// gets a dim `▎ ` left border bar with the body left at the default color
/// (tui-markdown emits no language highlighting), and an hr becomes a
/// full-width dim `─` rule.
///
/// Fence lines are classified from the SOURCE markdown (`fence_plan`): the
/// renderer normalizes every fence to exactly ``` so a nested example's inner
/// fences are invisible in the output — the source's marker lengths are the
/// only thing that keeps them straight. RESIDUAL LIMITATION: constructs the
/// plan's scan does not model (fences inside list items / blockquotes,
/// indented 4-space code blocks, entity-escaped info strings) make the plan
/// fail its verification against the rendered lines and fall back to the plain
/// toggle, which mis-tracks nested fences in exactly those cases — the
/// pre-existing behavior, kept as the safe degradation.
///
/// `width` is the conversation block's inner width (hr length). NOTE: the
/// rendered lines are memoized in MdCache keyed by row text, so a terminal
/// RESIZE keeps hr rules at the old width until the row re-renders — a minor
/// cosmetic staleness, accepted over keying the cache on width too.
pub(super) fn render_markdown(md: &str, width: usize) -> Vec<Line<'_>> {
    let raw = tui_markdown::from_str(md).lines;
    // Fence classification: prefer the SOURCE-derived plan (fence_plan —
    // fixes nested fences, whose inner ``` lines the normalized output can't
    // distinguish from real fences), but only after verifying it against the
    // actual rendered lines: every fence-looking rendered line must match the
    // plan's expected text, in order, with no leftovers. Any divergence (a
    // construct the scan doesn't model) falls back to the plain toggle,
    // which is exactly the pre-plan behavior.
    let plan = fence_plan(md);
    let plan_ok = {
        let mut plan_iter = plan.iter();
        let all_match = raw.iter().all(|line| {
            let first = line.spans.first().map(|s| s.content.as_ref()).unwrap_or("");
            if !first.trim_start().starts_with("```") {
                return true; // not fence-looking: doesn't consume the plan
            }
            matches!(plan_iter.next(), Some((_, want)) if want == first)
        });
        all_match && plan_iter.next().is_none()
    };
    let mut lines: Vec<Line<'_>> = Vec::with_capacity(raw.len());
    let mut in_fence = false;
    let mut next_evt = 0usize;
    for line in raw {
        let first: &str = line.spans.first().map(|s| s.content.as_ref()).unwrap_or("");
        if first.trim_start().starts_with("```") {
            // Fence-looking line: with a verified plan, classify from the
            // SOURCE (a Content line here is a code line wearing a fence's
            // clothes — it must fall through to the code rendering below,
            // NOT toggle); without one, the plain toggle.
            let is_content = if plan_ok {
                match plan[next_evt].0 {
                    FenceEvt::Open => {
                        in_fence = true;
                        false
                    }
                    FenceEvt::Close => {
                        in_fence = false;
                        false
                    }
                    FenceEvt::Content => true,
                }
            } else {
                in_fence = !in_fence;
                false
            };
            next_evt += 1;
            if !is_content {
                // Fence marker line (``` or ```lang): drop it.
                continue;
            }
        }
        if in_fence {
            // Code content: a dim `▎ ` border bar is the only signal — the
            // body keeps the default fg so code no longer fights the yellow
            // tool-call heads for attention (tui-markdown emits no inline
            // styling inside a fence anyway).
            let mut line = line;
            line.spans.insert(
                0,
                Span::styled("▎ ".to_string(), Style::default().fg(dim_fg())),
            );
            lines.push(line);
            continue;
        }
        let line_text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        if is_hrule(&line_text) {
            lines.push(Line::from(Span::styled(
                "─".repeat(width.max(1)),
                Style::default().fg(dim_fg()),
            )));
            continue;
        }
        let mut line = line;
        let first: String = line
            .spans
            .first()
            .map(|s| s.content.as_ref())
            .unwrap_or("")
            .to_string();
        if let Some(hashes) = header_level(&first) {
            // Collapse to one bold span; inline styling inside a header title
            // is rare, so dropping it is fine.
            let title: String = line
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()[hashes..]
                .trim_start()
                .to_string();
            let color = match hashes {
                1 => MD_H1_FG,
                2 | 3 => MD_H23_FG,
                _ => MD_DEEP_FG,
            };
            line.spans = vec![Span::styled(
                title,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )];
        } else if first.starts_with("- ") || first.starts_with("* ") || first.starts_with("+ ") {
            // tui-markdown isolates the marker in its own span, so swap just it
            // and keep the rest (inline code/bold on the item text).
            if let Some(s0) = line.spans.first_mut() {
                s0.content = "• ".into();
            }
        }
        redye_inline_code(&mut line, code_fg());
        lines.push(line);
    }
    lines
}

/// Re-dye tui-markdown's inline-code spans (fg White — invisible on a light
/// background) to `fg`. Pure, so the light/dark pair is testable without the
/// process-global theme knob.
pub(super) fn redye_inline_code(line: &mut Line, fg: Color) {
    for s in line.spans.iter_mut() {
        if s.style.fg == Some(Color::White) {
            s.style.fg = Some(fg);
        }
    }
}

/// Snapshot a `Line` with every span's borrowed content materialized into an
/// owned `String` → `Line<'static>`. Style and alignment are preserved (both
/// `Copy`). M6's render cache stores owned lines so it outlives the row text it
/// was rendered from (which is borrowed during the draw closure).
pub(super) fn line_to_owned(line: &Line) -> Line<'static> {
    Line {
        spans: line
            .spans
            .iter()
            .map(|s| Span {
                content: std::borrow::Cow::Owned(s.content.to_string()),
                style: s.style,
            })
            .collect(),
        style: line.style,
        alignment: line.alignment,
    }
}

/// SINGLE-slot cache of the CURRENT streaming row's rendered-markdown lines
/// (M6, re-scoped). The live zone re-renders the in-flight Assistant row on
/// every redraw — including each busy clock tick between deltas — which would
/// re-parse its markdown via `tui_markdown::from_str` each frame. The slot
/// memoizes exactly that one render keyed by (text, width); any other key
/// replaces it, so occupancy is bounded at 1 by construction.
///
/// Why one slot suffices (the original design was an unbounded `HashMap`):
/// the commit path renders each finalized row exactly once — `commit_rows`
/// runs only when the watermark advances, so a row is never re-rendered —
/// and those renders now BYPASS the cache (`push_assistant_body` checks
/// `live`), because a cached committed row is dead weight: its entry was
/// never read again yet stayed resident, and during streaming every TextDelta
/// inserted a fresh key (one full `Vec<Line>` of the message-so-far each),
/// accumulating O(text × deltas) memory per message. The content key keeps
/// the old anti-aliasing guarantee: equal-length different messages can
/// never share an entry, so a wrong cached render can't be committed into
/// the terminal scrollback (the bug the pre-content-key index-keyed cache
/// had — `build_history_lines` is called with different row slices across
/// frames, so relative indices pointed at different rows).
pub(super) struct MdCache {
    slot: Option<((String, usize), Vec<Line<'static>>)>,
}

impl MdCache {
    pub(super) fn new() -> Self {
        Self { slot: None }
    }
    pub(super) fn clear(&mut self) {
        self.slot = None;
    }
    /// Occupancy: always 0 or 1. Test hook for the unbounded-growth
    /// regression (`md_cache_bounded_single_slot_and_commit_bypass`).
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        usize::from(self.slot.is_some())
    }
    /// Cached rendered lines for `text` at `width`, recomputing via `render`
    /// when the slot's key differs. Keyed by (text, width) so equal-length
    /// different messages never alias; width participates because hr rules
    /// and code bars bake the available measure into their rendered lines
    /// (a terminal resize must not reuse the old geometry). The returned
    /// slice's borrow ends at the next `&mut self` call.
    pub(super) fn get_or_render(
        &mut self,
        text: &str,
        width: usize,
        render: impl FnOnce(&str) -> Vec<Line<'static>>,
    ) -> &[Line<'static>] {
        let hit = self
            .slot
            .as_ref()
            .is_some_and(|((t, w), _)| t == text && *w == width);
        if !hit {
            self.slot = Some(((text.to_string(), width), render(text)));
        }
        &self.slot.as_ref().expect("filled above on miss").1
    }
}

/// How one fence-looking line in tui-markdown's OUTPUT is classified — derived
/// from the SOURCE markdown by [`fence_plan`], because the output alone cannot
/// always tell (see `fence_plan` for why).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum FenceEvt {
    /// Opening fence: rendered as `"```" + info string`; enters a code block.
    Open,
    /// Closing fence: rendered bare `"```"`; leaves the code block.
    Close,
    /// A code CONTENT line that itself looks like a fence (the nested example
    /// case): rendered verbatim, must NOT toggle the in-code state.
    Content,
}

/// One SOURCE line's classification under the shared CommonMark fence scan
/// ([`fence_step`]). `Content` covers every line inside an open fence that
/// does not close it (fence-looking or not); `Plain` is everything outside.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum FenceLine {
    /// Opens a fence: `ch` is the marker char (`` ` `` or `~`), `len` its run
    /// length (≥ 3).
    Open { ch: char, len: usize },
    /// Closes the fence that is currently open.
    Close,
    /// A code-content line inside the open fence (it did not close it).
    Content,
    /// Not a fence line at all (outside a fence).
    Plain,
}

/// Classify one markdown SOURCE line against the currently open fence
/// (`open` = `(marker char, opener length)` inside a code block, `None`
/// outside), under CommonMark's fence rules: an OPENER is ≤3 spaces of
/// indent then ≥3 backticks or tildes — a backtick opener's info string may
/// not contain a backtick (the tilde-side restriction is deliberately
/// relaxed, matching the historical scan here); a CLOSER repeats the
/// opener's OWN character at ≥ its length with nothing but whitespace after.
/// Backtick and tilde fences never close each other: a ``` line inside a
/// ~~~ block is Content, and vice versa.
///
/// Shared by the render side ([`fence_plan`]) and the streaming split side
/// ([`last_safe_boundary`] / [`soft_cut_offset`]) so the two cannot drift
/// apart again — the split side used to flip its fence flag on any ```
/// prefix, silently cutting ~~~ blocks open mid-content.
pub(super) fn fence_step(line: &str, open: Option<(char, usize)>) -> FenceLine {
    // Spaces-only indent, mirroring the plan's model (a tab-indented
    // construct is not a fence here; the render caller's verification and
    // the split side's conservative in-fence treatment both stay safe).
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let backticks = rest.chars().take_while(|c| *c == '`').count();
    let tildes = rest.chars().take_while(|c| *c == '~').count();
    if let Some((ch, need)) = open {
        // Closer: same char, length ≥ the opener's, whitespace only after.
        let run = if ch == '`' { backticks } else { tildes };
        if indent <= 3 && run >= need && rest[run..].trim().is_empty() {
            return FenceLine::Close;
        }
        return FenceLine::Content;
    }
    if indent > 3 {
        return FenceLine::Plain;
    }
    // Opener: ≥3 markers; a backtick fence's info string must not contain
    // backticks (CommonMark).
    let (ch, run) = if backticks >= 3 {
        ('`', backticks)
    } else if tildes >= 3 {
        ('~', tildes)
    } else {
        return FenceLine::Plain;
    };
    if ch == '`' && rest[run..].trim().contains('`') {
        return FenceLine::Plain;
    }
    FenceLine::Open { ch, len: run }
}

/// Step the fence state over one SOURCE line: what `fence_step` classifies
/// translates 1:1 into the new open-fence state (`Some((char, length))`
/// inside a code block, `None` outside).
pub(super) fn fence_next(line: &str, open: Option<(char, usize)>) -> Option<(char, usize)> {
    match fence_step(line, open) {
        FenceLine::Open { ch, len } => Some((ch, len)),
        FenceLine::Close => None,
        FenceLine::Content | FenceLine::Plain => open,
    }
}

/// Precompute the sequence of fence-looking lines tui-markdown will emit, with
/// each classified from the SOURCE markdown.
///
/// Why the source is needed at all: tui-markdown 0.3 normalizes every fence to
/// EXACTLY three backticks in its output (`code_block_fence()` = `"```"`,
/// openers get the info string appended, closers are bare) — the original
/// marker length is gone. A 4-backtick fence containing a 3-backtick example
/// therefore renders its inner ``` content line indistinguishably from a real
/// fence, and a naive rendered-lines toggle flips on it: the rest of the outer
/// block renders as markdown and everything AFTER the outer close renders as
/// code. The source keeps the tick counts, so a CommonMark-style scan (see
/// [`fence_step`]) knows the inner line is content.
///
/// The plan is only an ACCELERATION-VERIFIED heuristic, not a second parser:
/// the caller checks it against the actual rendered lines (count + exact text)
/// and falls back to the plain toggle on any divergence. Divergence happens
/// for constructs this scan deliberately does not model — fences nested in
/// list items or blockquotes (rendered with prefix spans, so not
/// fence-looking anyway), indented (4-space) code blocks (which tui-markdown
/// also emits with ``` fence lines), HTML-entity info strings — and for those
/// the toggle's behavior is unchanged from before this fix.
pub(super) fn fence_plan(md: &str) -> Vec<(FenceEvt, String)> {
    /// Leading-space indent of `line` (spaces only; a tab-indented construct
    /// falls out of the plan's model and the caller's verification catches it).
    pub(super) fn indent_of(line: &str) -> usize {
        line.len() - line.trim_start_matches(' ').len()
    }
    let mut plan = Vec::new();
    // Open fence: (marker char, marker length). `None` outside code blocks.
    let mut open: Option<(char, usize)> = None;
    // Indent (0..=3 spaces) of the currently open fence — stripped from its
    // content lines, mirroring CommonMark/pulldown-cmark.
    let mut open_indent = 0usize;
    for line in md.split('\n') {
        let indent = indent_of(line);
        match fence_step(line, open) {
            FenceLine::Close => {
                plan.push((FenceEvt::Close, "```".to_string()));
                open = None;
            }
            FenceLine::Open { ch, len } => {
                let info = line[indent + len..].trim();
                plan.push((FenceEvt::Open, format!("```{info}")));
                open = Some((ch, len));
                open_indent = indent;
            }
            FenceLine::Content => {
                // Content: verbatim except the fence's own indent. Only lines
                // that LOOK like fences enter the plan (the caller only
                // matches those); the expected text is the rendered one.
                let strip = open_indent.min(indent);
                let stripped = &line[strip..];
                if stripped.starts_with("```") {
                    plan.push((FenceEvt::Content, stripped.to_string()));
                }
            }
            FenceLine::Plain => {}
        }
    }
    // An unclosed fence at EOF is auto-closed by pulldown-cmark, and
    // tui-markdown's end_codeblock then emits the bare closer — a streaming
    // half-written block must still match the rendered line stream.
    if open.is_some() {
        plan.push((FenceEvt::Close, "```".to_string()));
    }
    plan
}

/// ATX header level if `s` starts with 1..=6 '#' immediately followed by a
/// space (CommonMark). None otherwise.
fn header_level(s: &str) -> Option<usize> {
    let n = s.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&n) && s[n..].starts_with(' ') {
        Some(n)
    } else {
        None
    }
}
