use super::app::*;
use super::input::*;
use super::markdown::*;
use super::render::*;
use super::slash::*;
use super::theme::*;
use super::transcript::*;
use crate::config::Config;
use crate::provider::{Block, Message};
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

#[test]
fn markdown_fenced_code_block_styled_and_fences_dropped() {
    // Probe conclusion (tui-markdown 0.3): the ``` fence lines survive as
    // literal lines and the code between them is unstyled. render_markdown
    // must drop the fence lines and give the enclosed code a dim `▎ `
    // border bar; the body keeps the DEFAULT fg so code doesn't fight
    // the yellow tool-call heads for attention.
    let lines = render_markdown(
        "before\n\n```rust\nlet x = 1;\nlet y = 2;\n```\n\nafter `code`\n",
        40,
    );
    let texts: Vec<String> = lines.iter().map(row_text).collect();
    assert!(
        !texts.iter().any(|t| t.contains("```")),
        "fence lines leaked: {texts:?}"
    );
    let code: Vec<&Line> = lines
        .iter()
        .filter(|l| row_text(l).starts_with("▎ let "))
        .collect();
    assert_eq!(code.len(), 2, "code lines: {texts:?}");
    for l in code {
        // Span 0 is the dim border bar; the code spans stay default.
        assert_eq!(l.spans[0].content.as_ref(), "▎ ");
        assert_eq!(l.spans[0].style.fg, Some(dim_fg()));
        assert!(
            l.spans[1..].iter().all(|s| s.style.fg.is_none()),
            "code body must keep the default fg: {:?}",
            l.spans
        );
    }
    // Inline code outside a fence keeps tui-markdown's own styling
    // (fg White), untouched by the fence pass.
    let after = lines
        .iter()
        .find(|l| row_text(l).starts_with("after"))
        .expect("the line after the block");
    assert!(after.spans.iter().any(|s| s.style.fg == Some(Color::White)));
}

#[test]
fn markdown_nested_fences_render_from_the_source_plan() {
    // P2-4 regression: tui-markdown normalizes EVERY fence to exactly
    // three backticks in its output, so within the rendered lines alone
    // the inner ``` of a 4-backtick example is indistinguishable from a
    // real fence — the old per-line toggle flipped on it, rendering the
    // rest of the outer block as markdown and everything after the outer
    // close as code. The source-derived fence_plan keeps them straight.
    let md = "intro\n\n````md\nan example:\n```\ncode\n```\n````\n\nafter the block\n";
    let lines = render_markdown(md, 40);
    let texts: Vec<String> = lines.iter().map(row_text).collect();
    // No UNBARRED fence-looking line leaks (the inner ``` pair is real
    // code content and correctly keeps the `▎ ` code bar).
    assert!(
        !texts
            .iter()
            .any(|t| t.trim_start().starts_with("```") && !t.starts_with("▎ ")),
        "fence marker lines leaked: {texts:?}"
    );
    // Everything inside the OUTER fence is code — including the lines
    // between the inner ``` pair, which the toggle used to flip on.
    for want in ["▎ an example:", "▎ ```", "▎ code"] {
        assert!(
            texts.iter().any(|t| t == want),
            "missing code line {want:?}: {texts:?}"
        );
    }
    // Prose before and after renders WITHOUT the code bar.
    assert!(texts.iter().any(|t| t == "intro"), "{texts:?}");
    assert!(texts.iter().any(|t| t == "after the block"), "{texts:?}");
    // And no line after the block carries the bar (the old bug rendered
    // the post-block prose as code).
    let after_idx = texts
        .iter()
        .position(|t| t == "after the block")
        .expect("after line");
    assert!(
        !texts[after_idx..].iter().any(|t| t.starts_with("▎ ")),
        "prose after the block rendered as code: {texts:?}"
    );
}

#[test]
fn fence_plan_classifies_nested_unclosed_and_tilde_fences() {
    // Simple fenced block: open (with the info string) + close.
    let plan = fence_plan("```rust\nlet x = 1;\n```\n");
    assert_eq!(
        plan,
        vec![
            (FenceEvt::Open, "```rust".to_string()),
            (FenceEvt::Close, "```".to_string()),
        ]
    );
    // Nested: the 3-backtick lines inside a 4-backtick fence are CONTENT
    // (a closer needs ≥ the opener's length), and the EOF auto-close of
    // an unclosed fence still yields a Close event.
    let plan = fence_plan("````\nfoo\n```\n");
    assert_eq!(
        plan,
        vec![
            (FenceEvt::Open, "```".to_string()),
            (FenceEvt::Content, "```".to_string()),
            (FenceEvt::Close, "```".to_string()), // synthesized at EOF
        ]
    );
    // Tilde fences normalize to ``` in the output too; a backtick run
    // inside one is content, and a tilde run can't close a backtick
    // fence (different char) — nor can a SHORTER backtick run.
    let plan = fence_plan("~~~\n```\n~~~\n");
    assert_eq!(
        plan,
        vec![
            (FenceEvt::Open, "```".to_string()),
            (FenceEvt::Content, "```".to_string()),
            (FenceEvt::Close, "```".to_string()),
        ]
    );
    let plan = fence_plan("````\n```\n````\n");
    assert_eq!(
        plan,
        vec![
            (FenceEvt::Open, "```".to_string()),
            (FenceEvt::Content, "```".to_string()),
            (FenceEvt::Close, "```".to_string()),
        ]
    );
    // Prose-only and blank documents produce an empty plan.
    assert!(fence_plan("plain text\n\n- item").is_empty());
    assert!(fence_plan("").is_empty());
}

#[test]
fn markdown_hrule_becomes_a_full_width_dim_rule() {
    // tui-markdown emits a thematic break as a literal "---" line;
    // render_markdown swaps it for a dim ─ rule of the given width.
    let lines = render_markdown("above\n\n---\n\nbelow\n", 12);
    let rule = lines
        .iter()
        .find(|l| row_text(l).contains('─'))
        .expect("an hr line");
    assert_eq!(row_text(rule), "─".repeat(12));
    assert_eq!(rule.spans[0].style.fg, Some(dim_fg()));
    // is_hrule itself: ≥3 chars of -,*,_ only; "--" and "- item" are not.
    assert!(is_hrule("---") && is_hrule("***") && is_hrule("___"));
    assert!(is_hrule(" - - - "));
    assert!(!is_hrule("--") && !is_hrule("- item") && !is_hrule("abc---"));
}

// ── build_history_lines: history row styling ───────────────────────

/// Fixed inner width for history-building tests.
const TEST_W: usize = 40;

/// Shorthand for a collapsed tool call row.
fn tool_call_row(name: &str) -> Row {
    Row::ToolCall {
        name: name.into(),
        input: serde_json::Value::Null,
        collapsed: "arg summary".into(),
        expanded: false,
        started: Instant::now(),
    }
}

/// Shorthand for a collapsed tool result row (elapsed None, as when no
/// matching call row exists).
fn tool_result_row(name: &str, is_error: bool) -> Row {
    Row::ToolResult {
        name: name.into(),
        content: "body line".into(),
        collapsed: "body line".into(),
        is_error,
        expanded: false,
        elapsed: None,
    }
}

/// Build history lines with the test width, not streaming.
fn history(rows: &[Row]) -> Vec<Line<'static>> {
    let mut cache = MdCache::new();
    build_history_lines(rows, &mut cache, TEST_W, LiveRow::None, false, true)
}

#[test]
fn theme_light_from_matches_light_case_insensitively() {
    // Only the exact value "light" (ASCII case-insensitive) opts in;
    // unset / "dark" / empty / typo all keep the dark default.
    assert!(theme_light_from(Some("light")));
    assert!(theme_light_from(Some("Light")));
    assert!(theme_light_from(Some("LIGHT")));
    assert!(!theme_light_from(Some("dark")));
    assert!(!theme_light_from(Some("")));
    assert!(!theme_light_from(None));
}

#[test]
fn theme_fg_accessors_default_to_the_legacy_dark_constants() {
    // Tests never set THEME_LIGHT (OnceLock is process-global and cannot
    // be reset), so the accessors here exercise the unset → dark branch.
    // Pins "default zero drift": dark theme is byte-identical to the
    // pre-knob DIM_FG / White constants.
    assert_eq!(dim_fg(), Color::Gray);
    assert_eq!(code_fg(), Color::White);
}

#[test]
fn redye_inline_code_rewrites_only_white_on_black_spans() {
    let mut line = Line::from(vec![
        Span::styled("plain", Style::default()),
        Span::styled("code", Style::default().fg(Color::White).bg(Color::Black)),
        Span::styled("other", Style::default().fg(Color::Cyan)),
        Span::styled(
            "bold-code",
            Style::default()
                .fg(Color::White)
                .bg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("plain-white", Style::default().fg(Color::White)),
    ]);
    redye_inline_code(&mut line, MD_H23_FG, Color::Indexed(251));
    let styles: Vec<_> = line
        .spans
        .iter()
        .map(|s| (s.style.fg, s.style.bg))
        .collect();
    assert_eq!(
        styles,
        vec![
            (None, None),
            (Some(MD_H23_FG), Some(Color::Indexed(251))),
            (Some(Color::Cyan), None),
            (Some(MD_H23_FG), Some(Color::Indexed(251))),
            (Some(Color::White), None),
        ]
    );
    // Modifiers survive — only the inline-code colors are touched.
    assert!(line.spans[3].style.add_modifier.contains(Modifier::BOLD));
    // Dark theme passes White as the foreground target with a raised surface.
    let mut dark = Line::from(Span::styled(
        "code",
        Style::default().fg(Color::White).bg(Color::Black),
    ));
    redye_inline_code(&mut dark, Color::White, Color::Indexed(237));
    assert_eq!(dark.spans[0].style.fg, Some(Color::White));
    assert_eq!(dark.spans[0].style.bg, Some(Color::Indexed(237)));
}

#[test]
fn history_user_and_agent_use_inline_icons() {
    let rows = vec![Row::User("hello".into()), Row::Assistant("world".into())];
    let lines = history(&rows);

    // User: cyan-bold `❯ ` icon inline on the first body line — no
    // standalone header row. The user's own text earns the stronger weight.
    assert_eq!(row_text(&lines[0]), "❯ hello");
    let icon = lines[0].spans[0].style;
    assert_eq!(icon.fg, Some(USER_FG));
    assert!(icon.add_modifier.contains(Modifier::BOLD));
    assert!(lines[0].spans[1]
        .style
        .add_modifier
        .contains(Modifier::BOLD));

    // Assistant: green-bold `◆ ` icon the same way.
    assert_eq!(row_text(&lines[1]), "◆ world");
    let icon = lines[1].spans[0].style;
    assert_eq!(icon.fg, Some(AGENT_FG));
    assert!(icon.add_modifier.contains(Modifier::BOLD));
}

#[cfg(unix)]
#[test]
fn terminal_disconnect_probe_detects_pty_hangup() {
    // crossterm 0.29 spins inside event::poll on tty EOF. Exercise the
    // independent probe with a real pty pair, without touching test stdin.
    let mut master = -1;
    let mut slave = -1;
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
    assert!(!terminal_fd_disconnected(slave, Duration::ZERO).unwrap());
    unsafe { libc::close(master) };
    assert!(terminal_fd_disconnected(slave, Duration::from_millis(10)).unwrap());
    unsafe { libc::close(slave) };
}

#[test]
fn history_multiline_role_body_aligns_under_the_icon_text() {
    let rows = vec![Row::User("first\nsecond".into())];
    let lines = history(&rows);
    // Continuation lines indent to the icon's text column (2 cells).
    assert_eq!(row_text(&lines[0]), "❯ first");
    assert_eq!(row_text(&lines[1]), "  second");
}

#[test]
fn history_tool_rows_have_colored_heads_and_guided_bodies() {
    let rows = vec![
        tool_call_row("shell"),
        tool_result_row("shell", false),
        tool_result_row("shell", true),
    ];
    let lines = history(&rows);

    // Collapsed tools share a continuous amber/green/red execution rail.
    assert_eq!(row_text(&lines[0]), "    ▎ → shell  arg summary");
    let st = lines[0].spans[0].style;
    assert_eq!(st.fg, Some(TOOL_CALL_FG));
    assert!(st.add_modifier.contains(Modifier::BOLD));
    assert_eq!(lines[0].spans[1].style.fg, Some(dim_fg()));

    // Success stays compact and uses a dim preview. The ok head is NOT
    // bold: a green bold "✓" would read as a second "◆ " agent icon.
    assert_eq!(row_text(&lines[1]), "    ▎ ✓ shell  body line");
    let ok = lines[1].spans[0].style;
    assert_eq!(ok.fg, Some(TOOL_OK_FG));
    assert!(!ok.add_modifier.contains(Modifier::BOLD));
    assert_eq!(lines[1].spans[1].style.fg, Some(dim_fg()));

    // Error preview is red as well as carrying an explicit failure glyph;
    // the failure head alone earns the bold.
    assert_eq!(row_text(&lines[2]), "    ▎ ✗ shell  body line");
    let err = lines[2].spans[0].style;
    assert_eq!(err.fg, Some(TOOL_ERR_FG));
    assert!(err.add_modifier.contains(Modifier::BOLD));
    assert_eq!(lines[2].spans[1].style.fg, Some(TOOL_ERR_FG));
}

#[test]
fn history_tool_result_head_shows_elapsed_when_present() {
    let mut row = tool_result_row("shell", false);
    if let Row::ToolResult { elapsed, .. } = &mut row {
        *elapsed = Some("1.2s".into());
    }
    let lines = history(&[row]);
    assert_eq!(row_text(&lines[0]), "    ▎ ✓ shell · 1.2s  body line");
}

#[test]
fn history_collapsed_tool_preview_strips_ansi_without_losing_text() {
    let row = Row::ToolResult {
        name: "shell".into(),
        content: "\x1b[31m- old\x1b[0m".into(),
        collapsed: "\x1b[31m- old\x1b[0m".into(),
        is_error: false,
        expanded: false,
        elapsed: None,
    };
    let lines = history(&[row]);
    assert_eq!(row_text(&lines[0]), "    ▎ ✓ shell  - old");
    assert!(!row_text(&lines[0]).contains('\x1b'));
}

#[test]
fn history_tool_head_summary_wraps_with_a_hanging_indent() {
    // A wide collapsed summary on a narrow terminal wraps the head; the
    // continuation rows retain the execution gutter instead of falling back
    // to column 0. Wrapping is asserted through the same final pass the app uses.
    let mut row = tool_call_row("shell");
    if let Row::ToolCall { collapsed, .. } = &mut row {
        *collapsed = "alpha beta gamma delta epsilon zeta eta theta iota".into();
    }
    let lines = wrap_history(&history(&[row]), TEST_W);
    let body: Vec<&Line> = lines.iter().filter(|l| !row_text(l).is_empty()).collect();
    assert!(body.len() > 1, "head must wrap at width {TEST_W}");
    assert!(row_text(body[0]).starts_with("    ▎ → shell  alpha"));
    for cont in &body[1..] {
        assert!(
            row_text(cont).starts_with(TOOL_HEAD_MARK),
            "continuation lost the execution rail: {:?}",
            row_text(cont)
        );
        // The indent leaves room for text, not a full-width row.
        assert!(row_width(cont) <= TEST_W);
    }
    // All summary words survive the wrap.
    let all: String = body.iter().map(|l| row_text(l)).collect();
    assert!(all.contains("theta") && all.contains("iota"));
}

#[test]
fn history_system_levels_are_color_graded() {
    let rows = vec![
        Row::sys_info("[done: end_turn]"),
        Row::sys_warn("[cancelling…]"),
        Row::sys_error("[error] boom"),
    ];
    let lines = history(&rows);
    // Single-line layout: marker + text + blank per row. Info nests in the
    // tool gutter; warnings/errors rise to column 0 with distinct glyphs.
    let cases = [
        (0, dim_fg(), "    · "),
        (2, TOOL_CALL_FG, "! "),
        (4, TOOL_ERR_FG, "✗ "),
    ];
    for (base, fg, pad) in cases {
        // The "· " marker and the first text line share one rendered line.
        let (marker, body) = (&lines[base].spans[0], &lines[base].spans[1]);
        assert_eq!(marker.content.as_ref(), pad, "marker at {base}");
        assert_eq!(marker.style.fg, Some(fg), "marker at {base}");
        assert!(marker.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(body.style.fg, Some(fg), "body at {base}");
        assert!(!body.style.add_modifier.contains(Modifier::ITALIC));
    }
}

#[test]
fn history_system_multiline_indents_continuation_lines() {
    // Multi-line notices (e.g. the /help reply): first line carries the
    // "· " marker, continuation lines align under it — in the gutter for
    // info, at column 0 for warn/error.
    let rows = vec![Row::sys_info("first\nsecond\nthird")];
    let lines = history(&rows);
    assert_eq!(row_text(&lines[0]), "    · first");
    assert_eq!(row_text(&lines[1]), "      second");
    assert_eq!(row_text(&lines[2]), "      third");
    assert!(!lines[1].spans[0]
        .style
        .add_modifier
        .contains(Modifier::ITALIC));
    let lines = history(&[Row::sys_warn("w1\nw2")]);
    assert_eq!(row_text(&lines[0]), "! w1");
    assert_eq!(row_text(&lines[1]), "  w2");
}

#[test]
fn code_log_row_renders_with_exactly_one_marker() {
    // The CodeLog event used to bake a "· " into the sys_info payload
    // while the renderer (and render_transcript) draw their own marker —
    // "· · line". The payload must stay bare: exactly one marker on
    // screen, exactly one in /export.
    let row = code_log_row("console: hi".into());
    let lines = history(std::slice::from_ref(&row));
    assert_eq!(row_text(&lines[0]), "    · console: hi");
    assert!(!row_text(&lines[0]).contains("· ·"));
    let out = render_transcript(&[row]);
    assert!(out.contains("· console: hi"), "transcript: {out:?}");
    assert!(!out.contains("· ·"), "doubled marker leaked: {out:?}");
}

#[test]
fn history_turns_use_whitespace_instead_of_full_width_rules() {
    let rows = vec![
        Row::User("one".into()),
        Row::Assistant("a".into()),
        Row::User("two".into()),
    ];
    let lines = history(&rows);
    assert_eq!(row_text(&lines[0]), "❯ one");
    assert_eq!(row_text(&lines[1]), "◆ a");
    assert_eq!(row_text(&lines[2]), "");
    assert_eq!(row_text(&lines[3]), "❯ two");
    assert!(lines
        .iter()
        .all(|line| !row_text(line).contains(&"─".repeat(TEST_W))));
}

#[test]
fn history_first_user_after_the_welcome_line_gets_no_separator() {
    // rows[0] is the startup welcome (System), so the first User sits at
    // idx 1. The old `idx > 0` rule drew a separator above it that floated
    // mid-screen with nothing to separate — separators only between turns.
    let rows = vec![
        Row::sys_info(welcome_text("glm", 3)),
        Row::User("hi".into()),
    ];
    let lines = history(&rows);
    assert!(
        lines.iter().all(|l| row_text(l) != "─".repeat(TEST_W)),
        "no separator above the first User row"
    );
    // Welcome notice and user row are separated by whitespace, not a rule.
    assert_eq!(
        row_text(&lines[0]),
        format!("    · {}", welcome_text("glm", 3))
    );
    assert_eq!(row_text(&lines[2]), "❯ hi");
}

#[test]
fn queue_preview_is_render_only_and_reports_pause_state() {
    let queued = std::collections::VecDeque::from(vec![
        "next question with a long preview".to_string(),
        "last question".to_string(),
    ]);
    let line = queue_preview_line(&queued, false, 40).expect("non-empty queue preview");
    let text = row_text(&line);
    assert!(text.contains("2 queued"), "{text}");
    assert!(text.contains("next question"), "{text}");
    assert!(
        !text.contains("last question"),
        "only the FIFO head previews"
    );
    assert!(line.width() <= 40);

    let paused = row_text(&queue_preview_line(&queued, true, 40).expect("paused preview"));
    assert!(paused.contains("paused"), "{paused}");
    assert!(paused.contains("/resume"), "{paused}");
}

#[test]
fn detail_reprints_select_nth_last_without_mutating_history() {
    let first = Row::ToolCall {
        name: "read_file".into(),
        input: serde_json::json!({"path": "first"}),
        collapsed: "first".into(),
        expanded: false,
        started: Instant::now(),
    };
    let latest = Row::ToolCall {
        name: "shell".into(),
        input: serde_json::json!({"cmd": "printf hello"}),
        collapsed: "shell".into(),
        expanded: false,
        started: Instant::now(),
    };
    let rows = vec![first, tool_result_row("read_file", false), latest];

    let selected = nth_last_tool_call(&rows, 2).expect("second-last call");
    assert!(
        matches!(selected, Row::ToolCall { ref name, expanded: true, .. } if name == "read_file")
    );
    assert!(matches!(
        rows[0],
        Row::ToolCall {
            expanded: false,
            ..
        }
    ));
    assert!(nth_last_tool_call(&rows, 0).is_none());
    assert!(nth_last_tool_call(&rows, 3).is_none());

    let selected = latest_detail(&rows).expect("latest detail");
    assert!(matches!(selected, Row::ToolCall { ref name, expanded: true, .. } if name == "shell"));
}

#[test]
fn long_thinking_folds_on_commit_and_full_reprint_keeps_every_line() {
    let original = (1..=12)
        .map(|i| format!("reasoning line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let rows = vec![Row::Thinking {
        text: original.clone(),
        tight: false,
    }];
    let texts: Vec<String> = history(&rows).iter().map(row_text).collect();
    assert!(texts[0].starts_with("✻ Thinking · folded · "), "{texts:?}");
    assert!(texts[0].contains("/thinking"), "{texts:?}");
    assert_eq!(
        texts.len(),
        5,
        "header + three preview rows + spacer: {texts:?}"
    );
    assert!(!texts.iter().any(|t| t.trim() == "reasoning line 1"));
    assert!(texts.iter().any(|t| t.contains("reasoning line 10")));
    assert!(texts.iter().any(|t| t.contains("reasoning line 12")));

    // Export is lossless even though the committed screen representation folds.
    assert!(render_transcript(&rows).contains(&original));
    let full = nth_last_thinking(&rows, 1).expect("full thinking reprint");
    let full_texts: Vec<String> = history(&[full]).iter().map(row_text).collect();
    assert!(full_texts[0].contains(" · full · "), "{full_texts:?}");
    assert!(full_texts.iter().any(|t| t.trim() == "reasoning line 1"));
    assert!(full_texts.iter().any(|t| t.contains("reasoning line 12")));

    let boundary = (1..=8)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let short = history(&[Row::Thinking {
        text: boundary,
        tight: false,
    }]);
    assert!(!row_text(&short[0]).contains("folded"));
}

#[test]
fn thinking_fold_ignores_trailing_blank_rows() {
    // Real traces end with "\n" (often "\n\n"). The blank rows they produce
    // are not content: the count, the fold decision and the preview all go by
    // the trimmed trace.
    let trace = (1..=10)
        .map(|i| format!("reasoning line {i}\n"))
        .collect::<String>()
        + "\n";
    let rows = vec![Row::Thinking {
        text: trace.clone(),
        tight: false,
    }];
    let texts: Vec<String> = history(&rows).iter().map(row_text).collect();
    assert!(
        texts[0].contains("folded · 10 rows"),
        "ten content rows, not twelve: {texts:?}"
    );
    assert_eq!(
        texts.len(),
        5,
        "header + three preview rows + spacer: {texts:?}"
    );
    // All three preview rows carry content: the last three lines.
    for line in (8..=10).rev() {
        assert!(
            texts[1..4]
                .iter()
                .any(|t| t.trim() == format!("reasoning line {line}")),
            "{texts:?}"
        );
    }
    assert!(
        texts[1..4].iter().all(|t| !t.trim().is_empty()),
        "{texts:?}"
    );

    let full = nth_last_thinking(&rows, 1).expect("full thinking reprint");
    let full_texts: Vec<String> = history(&[full]).iter().map(row_text).collect();
    assert!(full_texts[0].contains("full · 10 rows"), "{full_texts:?}");

    // Exactly at the threshold the trailing blank rows must not tip an
    // eight-row trace into a fold.
    let boundary = (1..=8).map(|i| format!("line {i}\n")).collect::<String>();
    let short = history(&[Row::Thinking {
        text: boundary,
        tight: false,
    }]);
    assert!(!row_text(&short[0]).contains("folded"), "{short:?}");
}

#[test]
fn live_thinking_tail_never_starts_inside_a_grapheme() {
    // At the 128-grapheme live budget, this ZWJ sequence begins exactly at
    // the tail boundary. A char-based suffix starts at its ZWJ instead.
    let zwj = format!("{}👩‍💻{}", "x".repeat(20), "x".repeat(127));
    assert!(live_thinking_tail(&zwj, 1).starts_with("👩‍💻"));

    let heart = format!("{}❤️{}", "x".repeat(23), "x".repeat(127));
    assert!(live_thinking_tail(&heart, 1).starts_with("❤️"));
}

#[test]
fn new_slash_commands_share_grammar_and_busy_availability() {
    assert_eq!(classify("/queue"), SlashCmd::Queue);
    assert_eq!(classify("/unqueue"), SlashCmd::Unqueue);
    assert_eq!(classify("/resume"), SlashCmd::Resume);
    assert_eq!(classify("/call 2"), SlashCmd::Call(Some(2)));
    assert_eq!(classify("/thinking"), SlashCmd::Thinking(None));
    assert!(matches!(classify("/call 0"), SlashCmd::Unknown(_)));
    assert!(matches!(classify("/thinking nope"), SlashCmd::Unknown(_)));
    assert!(matches!(classify("/queue now"), SlashCmd::Unknown(_)));

    let mut popup = SlashPopup::new();
    popup.update_for("/", true);
    assert!(popup.is_open());
    assert!(!popup.matches.iter().any(|spec| spec.name == "clear"));
    assert!(!popup.matches.iter().any(|spec| spec.name == "context"));
    let lines = slash_popup_lines_for(&popup, 4, 24);
    assert_eq!(lines.len(), 4, "three command rows plus footer");
    assert!(lines.iter().all(|line| line.width() <= 24));
    let footer = row_text(lines.last().unwrap());
    assert!(footer.contains("Tab") && footer.contains("Enter"));
}

#[test]
fn empty_state_and_semantic_status_fit_narrow_cells() {
    for width in [0, 1, 12, 40, 100] {
        let lines = empty_state_lines(width);
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|line| line.width() <= width));
    }
    let opening = empty_state_lines(80);
    let masthead = row_text(&opening[0]);
    assert!(masthead.contains("T I N Y A G E N T"));
    assert!(masthead.contains(env!("CARGO_PKG_VERSION")));
    assert!(!masthead.contains("TERMINAL AGENT"));
    assert_eq!(
        opening[0].width(),
        80,
        "version pins to the chrome's right edge"
    );
    assert!(opening[0]
        .spans
        .iter()
        .any(|span| span.style.fg == Some(AGENT_FG)));
    assert_eq!(opening[1].width(), 80, "rule spans the whole work area");
    assert!(row_text(&opening[1]).contains("━"));
    assert!(row_text(&opening[2]).contains("real terminal work"));
    // One key legend only: the composer's bottom border. The welcome sheet
    // must not repeat it two rows above.
    let footer = composer_footer(false, 80).expect("footer fits at 80");
    assert!(row_text(&footer).contains("Enter"));
    assert!(row_text(&footer).contains("commands"));
    assert!(row_text(&footer).contains("/help"));
    assert!(composer_footer(true, 80).is_none());
    assert!(composer_footer(false, 20).is_none());

    let failed = build_status_line("model", "failed", false, None, None, "", "", 5);
    assert_eq!(row_text(&failed), "fail…");
    assert_eq!(failed.spans[0].style.fg, Some(TOOL_ERR_FG));
    let cancelling = build_status_line("model", "cancelling", true, None, None, "", "", 40);
    assert!(row_text(&cancelling).contains("cancelling"));
    assert_eq!(cancelling.spans[1].style.fg, Some(TOOL_CALL_FG));

    assert_eq!(dim_fg_for(false), Color::Gray);
    assert_eq!(dim_fg_for(true), Color::DarkGray);
    assert_eq!(code_fg_for(false), Color::White);
    assert_eq!(code_fg_for(true), MD_H23_FG);
}

#[test]
fn status_stats_stay_silent_until_something_was_billed() {
    // A fresh session must not advertise "0 in / 0 out".
    assert_eq!(status_stats(0, 0, 0, 0), "");
    // Any billed counter (including cache-only traffic) wakes the segment.
    assert!(status_stats(1, 0, 0, 0).contains("1 in"));
    assert!(status_stats(0, 0, 1200, 0).contains("cache 1.2k w"));
    assert!(status_stats(0, 0, 0, 900).contains("900 r"));
}

#[test]
fn queue_preview_never_exceeds_even_a_tiny_viewport() {
    let queued = std::collections::VecDeque::from(vec!["你好🙂".to_string()]);
    assert!(queue_preview_line(&std::collections::VecDeque::new(), false, 20).is_none());
    for width in 0..24 {
        let line = queue_preview_line(&queued, true, width).unwrap();
        assert!(line.width() <= width, "width {width}: {}", row_text(&line));
    }
}

#[test]
fn empty_state_and_slash_popup_draw_into_a_real_cell_grid() {
    let term = test_line_terminal(empty_state_lines(40), 40, 4);
    let first = test_buffer_row(&term, 0);
    let third = test_buffer_row(&term, 2);
    let fourth = test_buffer_row(&term, 3);
    assert!(first.contains("T I N Y A G E N T"), "{first:?}");
    assert!(third.contains("Ask. Build. Debug."), "{third:?}");
    assert!(
        fourth.trim().is_empty(),
        "one breathing row above the composer"
    );
    assert!(first.width() <= 40);
    // The empty sheet carries no keycaps; the composer border owns the only
    // legend (and at 40 columns it still fits the short form).
    assert!(!first.contains("Enter"), "{first:?}");
    assert!(composer_footer(false, 38).is_some());

    let mut popup = SlashPopup::new();
    popup.update_for("/c", false);
    let lines = slash_popup_lines_for(&popup, 4, 40);
    let term = test_line_terminal(lines, 40, 4);
    let cells = term.backend().buffer();
    assert!(test_buffer_row(&term, 0).contains("/c"));
    assert_eq!(cells[(0, 0)].style().bg, Some(keycap_bg()));
    assert!(test_buffer_row(&term, 3).contains("select"));
}

#[test]
fn history_streaming_cursor_marks_the_live_assistant_tail() {
    let rows = vec![
        Row::Assistant("done".into()),
        Row::Assistant("typing".into()),
    ];
    let mut cache = MdCache::new();
    let lines = build_history_lines(&rows, &mut cache, TEST_W, LiveRow::Text, false, true);
    // Adjacent assistant rows get a gap (split chunks recover their
    // paragraph separator; separate messages stop reading as one blob).
    assert_eq!(row_text(&lines[0]), "◆ done");
    assert_eq!(row_text(&lines[1]), "");
    assert_eq!(row_text(&lines[2]), "◆ typing▌");
    let last = lines[2].spans.last().expect("a cursor span");
    assert_eq!(last.content.as_ref(), "▌");
    assert_eq!(last.style.fg, Some(AGENT_FG));
    // Not streaming: no cursor anywhere.
    let lines = history(&rows);
    assert!(!row_text(&lines[1]).contains('▌'));
}

#[test]
fn history_thinking_appends_a_transient_placeholder_tail() {
    let rows = vec![Row::User("hi".into())];
    let mut cache = MdCache::new();
    let lines = build_history_lines(&rows, &mut cache, TEST_W, LiveRow::None, true, true);
    let last = lines.last().expect("a thinking tail line");
    assert_eq!(row_text(last), "◆ …");
    let icon = last.spans[0].style;
    assert_eq!(icon.fg, Some(AGENT_FG));
    assert!(icon.add_modifier.contains(Modifier::BOLD));
    assert_eq!(last.spans[1].style.fg, Some(dim_fg()));
    // Not thinking: no tail, the trailing spacer stays last.
    let lines = history(&rows);
    assert_eq!(row_text(lines.last().unwrap()), "");
    // The placeholder is row-independent: with no rows at all it is the
    // whole render.
    let mut cache = MdCache::new();
    let lines = build_history_lines(&[], &mut cache, TEST_W, LiveRow::None, true, true);
    assert_eq!(lines.len(), 1);
    assert_eq!(row_text(&lines[0]), "◆ …");
}

#[test]
fn thinking_row_uses_a_distinct_italic_channel_without_markdown() {
    // The trace must NOT read like the answer: no markdown parsing
    // (asterisks stay literal), its own magenta italic channel, `✻ ` marker.
    // (A trailing blank spacer line follows every row boundary — these
    // assertions target the body lines, not lines.last().)
    let rows = vec![Row::Thinking {
        text: "**bold** and `code`".into(),
        tight: false,
    }];
    let lines = history(&rows);
    assert_eq!(row_text(&lines[0]), "✻ **bold** and `code`");
    let m = lines[0].spans[0].style;
    assert_eq!(m.fg, Some(THINKING_FG));
    assert!(m.add_modifier.contains(Modifier::ITALIC));
    let body = lines[0].spans[1].style;
    assert_eq!(body.fg, Some(THINKING_FG));
    assert!(body.add_modifier.contains(Modifier::ITALIC));

    // Multi-line traces: continuation lines indent under the marker.
    let rows = vec![Row::Thinking {
        text: "first\nsecond".into(),
        tight: false,
    }];
    let lines = history(&rows);
    assert_eq!(row_text(&lines[0]), "✻ first");
    assert_eq!(row_text(&lines[1]), "  second");

    // A sub-agent's trace carries its name on the head line.
    let rows = vec![Row::Thinking {
        text: "[worker] planning".into(),
        tight: false,
    }];
    let lines = history(&rows);
    assert_eq!(row_text(&lines[0]), "✻ [worker] planning");

    // Whitespace-only traces render nothing but the boundary spacer —
    // no stray bare marker.
    let lines = history(&[Row::Thinking {
        text: "  \n ".into(),
        tight: false,
    }]);
    assert!(lines.iter().all(|l| row_text(l).trim().is_empty()));
}

#[test]
fn thinking_row_gets_a_cursor_only_while_it_is_the_live_row() {
    let rows = vec![Row::Thinking {
        text: "reasoning".into(),
        tight: false,
    }];
    let mut cache = MdCache::new();
    let lines = build_history_lines(&rows, &mut cache, TEST_W, LiveRow::Think, false, true);
    // (lines.last() is the row-boundary spacer; the cursor sits on the
    // last BODY line.)
    assert!(
        lines.iter().any(|l| row_text(l).ends_with('▌')),
        "cursor on the live trace: {lines:?}"
    );
    // Text-live (an Assistant streaming) must NOT cursor a Thinking row.
    let lines = build_history_lines(&rows, &mut cache, TEST_W, LiveRow::Text, false, true);
    assert!(!lines.iter().any(|l| row_text(l).contains('▌')));
    // Committed render: no cursor.
    let lines = history(&rows);
    assert!(!lines.iter().any(|l| row_text(l).contains('▌')));
}

#[test]
fn assistant_cont_renders_without_icon_and_gets_the_live_cursor() {
    // A continuation chunk: body indented under the head's icon column,
    // no `◆ ` of its own, a gap between the chunks (the split consumed
    // the paragraph separator — the gap restores it).
    let rows = vec![
        Row::Assistant("head".into()),
        Row::AssistantCont {
            text: "tail".into(),
            tight: false,
        },
    ];
    let lines = history(&rows);
    let texts: Vec<String> = lines.iter().map(row_text).collect();
    // (trailing blank = end-of-history spacer)
    assert_eq!(
        texts,
        vec![
            "◆ head".to_string(),
            String::new(),
            "  tail".to_string(),
            String::new()
        ]
    );

    // The Cont chunk is the live row while it streams: cursor works.
    let mut cache = MdCache::new();
    let lines = build_history_lines(&rows, &mut cache, TEST_W, LiveRow::Text, false, true);
    assert_eq!(row_text(&lines[2]), "  tail▌");

    // The trace/answer pair separates (blank after ✻, tight under ❯).
    let rows = vec![
        Row::User("q".into()),
        Row::Thinking {
            text: "trace".into(),
            tight: false,
        },
        Row::Assistant("answer".into()),
    ];
    let texts: Vec<String> = history(&rows).iter().map(row_text).collect();
    assert_eq!(
        texts,
        vec![
            "❯ q".to_string(),
            "✻ trace".to_string(),
            String::new(),
            "◆ answer".to_string(),
            String::new(),
        ]
    );
}

#[test]
fn render_transcript_merges_cont_chunks_without_repeat_headers() {
    let rows = vec![
        Row::User("hi".into()),
        Row::Assistant("first".into()),
        Row::AssistantCont {
            text: "second".into(),
            tight: false,
        },
    ];
    let out = render_transcript(&rows);
    assert!(out.contains("Tiny\nfirst\n\nsecond"), "merged: {out:?}");
    assert_eq!(out.matches("Tiny").count(), 1, "chunk repeated the header");
}

#[test]
fn commit_and_inflight_cover_cont_chunks() {
    // The streaming Cont tail is held back while open, committed closed.
    let rows = vec![
        Row::User("hi".into()),
        Row::Assistant("p1".into()),
        Row::AssistantCont {
            text: "p2".into(),
            tight: false,
        },
    ];
    assert_eq!(commit_pending_end(&rows, 0, true), 2);
    assert_eq!(commit_pending_end(&rows, 0, false), 3);
    // Live zone points at the Cont tail while it streams.
    assert_eq!(inflight_row_index(&rows, true), Some(2));
}

#[test]
fn pending_input_does_not_hold_back_real_transcript_rows() {
    let queued = std::collections::VecDeque::from(vec!["later".to_string()]);
    let rows = vec![Row::Assistant("answer".into()), Row::sys_info("finished")];
    // Queue state lives outside `rows`, so every real row can commit even
    // while unsent input remains pending.
    assert_eq!(queued.len(), 1);
    assert_eq!(commit_pending_end(&rows, 0, false), 2);
}

#[test]
fn fmt_elapsed_switches_to_minutes_at_60s() {
    assert_eq!(fmt_elapsed(0), "0s");
    assert_eq!(fmt_elapsed(59), "59s");
    assert_eq!(fmt_elapsed(60), "1m00s");
    assert_eq!(fmt_elapsed(245), "4m05s");
    assert_eq!(fmt_elapsed(3600), "60m00s");
}

#[test]
fn ctrl_c_double_press_confirms_within_the_window() {
    let now = Instant::now();
    // Not armed yet: the first press only arms.
    assert!(!ctrl_c_should_quit(None, now));
    // Armed just now / inside the 2s window: confirms.
    assert!(ctrl_c_should_quit(Some(now), now));
    assert!(ctrl_c_should_quit(
        Some(now - Duration::from_millis(1990)),
        now
    ));
    // Past the window: this press re-arms rather than quits.
    assert!(!ctrl_c_should_quit(
        Some(now - Duration::from_millis(2100)),
        now
    ));
}

#[test]
fn is_normal_stop_covers_both_provider_families() {
    // Anthropic: end_turn / tool_use / stop_sequence; OpenAI: stop /
    // tool_calls — none of these get a "[done: …]" row. `cancelled` is
    // silent too: the Esc/Ctrl-C path already printed "[cancelling…]".
    for normal in [
        "end_turn",
        "tool_use",
        "stop",
        "tool_calls",
        "stop_sequence",
        "cancelled",
    ] {
        assert!(is_normal_stop(normal), "{normal} should be silent");
    }
    // Abnormal stops DO get a warn row — including the stuck-loop
    // guardrail's `stuck_loop` (a terminated run the user should see).
    for abnormal in ["max_tokens", "length", "error", "unknown", "", "stuck_loop"] {
        assert!(!is_normal_stop(abnormal), "{abnormal} should be shown");
    }
}

#[test]
fn welcome_text_names_the_ready_session() {
    assert_eq!(
        welcome_text("glm-4.6", 7),
        "session ready · glm-4.6 · 7 tools"
    );
}

#[test]
fn input_title_merges_activity_elapsed_cancel_and_queue() {
    assert_eq!(
        build_input_title(true, '⠋', "running shell", 65, 2, 200),
        " ⠋ running shell… · 1m05s · Ctrl-C cancel · 2 queued "
    );
    assert_eq!(build_input_title(false, '⠋', "", 0, 0, 200), " ◆ MESSAGE ");
}

#[test]
fn input_title_drops_activity_before_elapsed_cancel_and_queue() {
    // ratatui clips an over-long title from the right — which would eat
    // exactly the elapsed/cancel/queue tail. The title degrades instead:
    // the activity name goes first.
    let full = build_input_title(true, '⠋', "running shell", 65, 2, 200);
    // Exactly at the boundary the full title still fits; one column less
    // degrades to spinner + elapsed + cancel + queue.
    assert_eq!(
        build_input_title(true, '⠋', "running shell", 65, 2, full.width()),
        full
    );
    assert_eq!(
        build_input_title(true, '⠋', "running shell", 65, 2, full.width() - 1),
        " ⠋ 1m05s · Ctrl-C cancel · 2 queued "
    );
    // Extremely narrow: elapsed is expendable, the cancel affordance is not.
    assert_eq!(
        build_input_title(true, '⠋', "thinking", 8, 0, 17),
        " ⠋ Ctrl-C cancel "
    );
    let tiny = build_input_title(true, '⠋', "thinking", 8, 0, 10);
    assert_eq!(tiny.width(), 10);
    assert!(tiny.contains("Ctrl"), "{tiny}");
}

#[test]
fn failed_title_names_the_action_that_exists() {
    // Nothing held: /resume would only answer "[no paused queue]", so the
    // title points at the composer.
    let bare = failed_title(0, 200);
    assert!(bare.contains("failed"), "{bare}");
    assert!(!bare.contains("/resume"), "{bare}");
    assert!(bare.contains("retry"), "{bare}");

    // A held queue is the one state in which /resume does something.
    let held = failed_title(2, 200);
    assert!(held.starts_with(" failed · /resume "), "{held}");

    // Exactly at the boundary the full title fits; one column less degrades
    // to the short form, which still carries the hint and never overflows.
    assert_eq!(failed_title(2, held.width()), held);
    let short = failed_title(2, held.width() - 1);
    assert_eq!(short, " failed · /resume ");
    assert_eq!(failed_title(0, bare.width()), bare);
    assert_eq!(failed_title(0, bare.width() - 1), " failed ");
    for width in 0..=12 {
        assert!(failed_title(2, width).width() <= width);
        assert!(failed_title(0, width).width() <= width);
    }
}

#[test]
fn queue_pause_is_a_property_of_held_messages() {
    // A failed run pauses only what is actually held: nothing queued means
    // nothing to pause (a stale flag would hold messages queued during the
    // next healthy run).
    assert!(!pause_for_held(true, 0));
    assert!(pause_for_held(true, 1));
    // A withdrawal never pauses a live queue, and lifts the pause together
    // with the last held message.
    assert!(!pause_for_held(false, 3));
    assert!(!pause_for_held(true, 0));
    assert!(pause_for_held(true, 2));
}

#[test]
fn auto_expand_result_opens_short_failures_only() {
    // Successes never auto-open, however short.
    assert!(!auto_expand_result(false, "one line"));
    // A short failure opens; the boundary is 20 lines inclusive.
    let short = (1..=20)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(auto_expand_result(true, &short));
    let long = (1..=21)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!auto_expand_result(true, &long));
}

#[test]
fn fmt_tool_elapsed_one_decimal_below_10s() {
    assert_eq!(fmt_tool_elapsed(Duration::from_millis(1200)), "1.2s");
    assert_eq!(fmt_tool_elapsed(Duration::from_millis(9_940)), "9.9s");
    assert_eq!(fmt_tool_elapsed(Duration::from_secs(10)), "10s");
    assert_eq!(fmt_tool_elapsed(Duration::from_secs(65)), "65s");
}

#[test]
fn tool_running_tracks_calls_fifo_and_clears() {
    // Render-only tracker: on_call pushes, on_result removes the first
    // same-name entry (falling back to the oldest), clear/empty no-op.
    let mut t = ToolRunning::new();
    assert!(t.calls.is_empty());
    t.on_call("shell");
    t.on_call("read");
    t.on_call("shell"); // parallel same-name call
    assert_eq!(t.calls.len(), 3);
    t.on_result("shell"); // first same-name (oldest shell) drops
    assert_eq!(t.calls.len(), 2);
    assert_eq!(t.calls[0].0, "read");
    t.on_result("missing"); // no same-name → oldest drops
    assert_eq!(t.calls[0].0, "shell");
    t.on_result("shell");
    t.on_result("shell"); // already empty → no-op, not a panic
    assert!(t.calls.is_empty());
    t.on_call("x");
    t.clear();
    assert!(t.calls.is_empty());
}

#[test]
fn tool_running_line_shows_name_elapsed_and_parallel_count() {
    let now = Instant::now();
    let mut t = ToolRunning::new();
    assert!(tool_running_line(&t, now).is_none(), "nothing in flight");
    t.calls = vec![("shell".into(), now - Duration::from_millis(3200))];
    let l = tool_running_line(&t, now).expect("a placeholder line");
    assert_eq!(l.spans[0].content.as_ref(), "    ▎ → shell");
    assert_eq!(l.spans[0].style.fg, Some(TOOL_CALL_FG));
    assert!(l.spans[0].style.add_modifier.contains(Modifier::BOLD));
    assert_eq!(l.spans[1].content.as_ref(), " · 3.2s…");
    assert_eq!(l.spans[1].style.fg, Some(dim_fg()));
    // Parallel calls aggregate: the OLDEST call's elapsed + (+N) count.
    t.calls.push(("read".into(), now));
    let l = tool_running_line(&t, now).expect("a placeholder line");
    assert_eq!(l.spans[0].content.as_ref(), "    ▎ → shell");
    assert_eq!(l.spans[1].content.as_ref(), " · 3.2s… (+1)");
}

#[test]
fn tool_elapsed_for_matches_nearest_same_name_call_backwards() {
    let now = Instant::now();
    let call_at = |name: &str, ago: Duration| Row::ToolCall {
        name: name.into(),
        input: serde_json::Value::Null,
        collapsed: String::new(),
        expanded: false,
        started: now - ago,
    };
    let rows = vec![
        call_at("shell", Duration::from_secs(5)),
        call_at("read", Duration::from_secs(3)),
        call_at("shell", Duration::from_millis(1500)),
    ];
    // Nearest-back "shell" is the 1.5s one, not the 5s one.
    assert_eq!(tool_elapsed_for(&rows, "shell"), Some("1.5s".into()));
    assert_eq!(tool_elapsed_for(&rows, "read"), Some("3.0s".into()));
    // No matching call row (resumed session) → None, header hides it.
    assert_eq!(tool_elapsed_for(&rows, "missing"), None);
}

#[test]
fn build_status_line_reduces_detail_on_narrow_terminals() {
    let stats = " · 5 in · 6 out";
    // Wide: full token detail appears as a dim third span.
    let wide = build_status_line("glm", "idle", false, None, None, stats, "", 200);
    assert_eq!(wide.spans.len(), 3);
    assert_eq!(wide.spans[2].content.as_ref(), stats);
    assert_eq!(wide.spans[2].style.fg, Some(dim_fg()));
    // Medium: stats no longer fit, so the bar drops to badge+state only.
    let medium_width = " ◆ glm".width() + "  ● ready".width();
    let medium = build_status_line("glm", "idle", false, None, None, stats, "", medium_width);
    assert_eq!(medium.spans.len(), 2);
    // Narrow: model and live state remain, everything else disappears.
    let narrow = build_status_line("glm", "idle", false, None, None, stats, "", medium_width);
    assert_eq!(narrow.spans.len(), 2);
    // Busy and ready share the agent accent; the run phase remains explicit
    // in text (running vs ready), never color-only. Idle carries no clock.
    let busy = build_status_line("glm", "running", true, None, None, stats, "", 200);
    let st = busy.spans[1].style;
    assert_eq!(st.fg, Some(AGENT_FG));
    assert!(st.add_modifier.contains(Modifier::BOLD));
    let idle = build_status_line("glm", "idle", false, None, None, stats, "", 200);
    assert!(row_text(&idle).contains("● ready"));
    assert_eq!(idle.spans[1].style.fg, Some(AGENT_FG));
    // The model is a compact text label on the shared status surface, not a
    // terminal-width-competing solid cyan block.
    assert_eq!(wide.spans[0].style.fg, Some(USER_FG));
    assert_eq!(wide.spans[0].style.bg, None);
    assert!(wide.spans[0].style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn build_status_line_ctx_outlives_the_stats() {
    let stats = " · 5 in · 6 out";
    let ctx = Some(("ctx 21% (54.7k/256k)", Style::default().fg(dim_fg())));
    // Wide: ctx sits between the state and the stats, keeping its style.
    let wide = build_status_line("glm", "idle", false, ctx, None, stats, "", 200);
    assert_eq!(wide.spans.len(), 4);
    assert_eq!(wide.spans[2].content.as_ref(), " · ctx 21% (54.7k/256k)");
    assert_eq!(wide.spans[2].style.fg, Some(dim_fg()));
    assert_eq!(wide.spans[3].content.as_ref(), stats);
    // Narrower: stats drop out but ctx stays — it's the most decision-relevant
    // number, so it outlives the stats and is the last segment standing.
    let ctx_only_width = " ◆ glm".width() + "  ● ready".width() + " · ctx 21% (54.7k/256k)".width();
    let minimal = build_status_line("glm", "idle", false, ctx, None, stats, "", ctx_only_width);
    assert_eq!(minimal.spans.len(), 3);
    assert_eq!(minimal.spans[2].content.as_ref(), " · ctx 21% (54.7k/256k)");
}

#[test]
fn build_status_line_plan_segment_drops_after_stats_before_ctx() {
    // The plan segment sits between ctx and stats; narrowing drops stats
    // first, then plan, then ctx (ctx survives longest). plan=None ⇒ the
    // segment is absent and behavior matches pre-plan exactly.
    let stats = " · 5 in · 6 out";
    let ctx = Some(("ctx 21%", Style::default().fg(dim_fg())));
    let plan = Some((" · plan 2/5", Style::default().fg(TOOL_CALL_FG)));
    // Wide: all four optional segments present, plan between ctx and stats.
    let wide = build_status_line("glm", "idle", false, ctx, plan, stats, "", 200);
    assert_eq!(wide.spans.len(), 5);
    assert_eq!(wide.spans[2].content.as_ref(), " · ctx 21%");
    assert_eq!(wide.spans[3].content.as_ref(), " · plan 2/5");
    assert_eq!(wide.spans[3].style.fg, Some(TOOL_CALL_FG));
    assert_eq!(wide.spans[4].content.as_ref(), stats);
    // Tight: stats drop, ctx+plan remain (plan outlives stats).
    let no_stats_w =
        " ◆ glm".width() + "  ● ready".width() + " · ctx 21%".width() + " · plan 2/5".width();
    let mid = build_status_line("glm", "idle", false, ctx, plan, stats, "", no_stats_w);
    assert_eq!(mid.spans.len(), 4);
    assert_eq!(mid.spans[3].content.as_ref(), " · plan 2/5");
    // Tighter: plan drops too, ctx alone survives (ctx outlives plan).
    let ctx_only_w = " ◆ glm".width() + "  ● ready".width() + " · ctx 21%".width();
    let tight = build_status_line("glm", "idle", false, ctx, plan, stats, "", ctx_only_w);
    assert_eq!(tight.spans.len(), 3);
    assert_eq!(tight.spans[2].content.as_ref(), " · ctx 21%");
}

#[test]
fn right_path_seg_right_aligns_truncates_and_vanishes() {
    // Fits: right-aligned with padding, dim.
    let seg = right_path_seg("/tmp/proj", 20).expect("fits");
    assert_eq!(seg.content.as_ref(), "           /tmp/proj");
    assert_eq!(seg.style.fg, Some(dim_fg()));
    // Under 12 columns left: no segment at all.
    assert!(right_path_seg("/tmp/proj", 11).is_none());
    assert_eq!(
        right_path_seg("/tmp/proj", 12)
            .map(|s| s.content.into_owned())
            .as_deref(),
        Some("   /tmp/proj")
    );
    // Too long for the budget: keep complete trailing directory components,
    // never a chopped fragment such as "de-code".
    let seg =
        right_path_seg("/Users/chenpu/workspace/claude-code/TinyAgent", 20).expect("truncated");
    assert_eq!(seg.content.as_ref(), "         …/TinyAgent");
    assert_eq!(seg.content.width(), 20);
    // Even "…" + the basename exceeds the budget: nothing.
    assert!(right_path_seg("/a/verylongbasename", 12).is_none());
    // Windows backslash paths: the basename is the segment after EITHER
    // separator — rsplit('/')-only found no split, took the whole path as
    // the "basename", and the segment vanished (regression).
    let seg = right_path_seg(r"D:\work\spaces\tiny-agent", 20).expect("win path truncates");
    assert!(seg.content.contains('…'), "{:?}", seg.content);
    assert!(
        seg.content.ends_with("tiny-agent"),
        "the project dir tail must survive: {:?}",
        seg.content
    );
    assert_eq!(seg.content.width(), 20);
    // Mixed separators take the last segment regardless of kind.
    let seg = right_path_seg(r"C:/mixed\path\proj", 40).expect("fits");
    assert!(seg.content.ends_with(r"C:/mixed\path\proj"));
    assert_eq!(seg.content.width(), 40);
    // Very wide bars still cap the path itself: padding aligns the tail, but
    // a 90-cell absolute path must not become the dominant visual element.
    let seg = right_path_seg(
        "/private/var/folders/35/7wcyk36s4hd9l_gf49fb45x40000gn/T/tinyagent-tui-demo",
        100,
    )
    .expect("wide path");
    let visible = seg.content.trim_start();
    assert!(visible.width() <= 48, "{visible:?}");
    assert!(visible.starts_with('…'), "{visible:?}");
    assert!(visible.ends_with("tinyagent-tui-demo"));
    assert!(
        !visible.contains("7wcyk36"),
        "no random partial parent: {visible:?}"
    );
    assert_eq!(seg.content.width(), 100);
    // Empty path (cwd query failed): nothing.
    assert!(right_path_seg("", 40).is_none());
}

#[test]
fn build_status_line_path_takes_what_the_left_side_leaves() {
    let stats = " · 5 in · 6 out";
    // The model label + ready state + stats use 30 columns; at 48 the path
    // right-aligns into the remaining 18.
    let line = build_status_line("glm", "idle", false, None, None, stats, "/tmp/proj", 48);
    let last = line.spans.last().expect("a path span");
    assert!(last.content.ends_with("/tmp/proj"));
    assert_eq!(line.width(), 48, "left + padded path fills the bar");
    // Narrow terminal: stats still fit but leave no useful room for the path.
    let line = build_status_line("glm", "idle", false, None, None, stats, "/tmp/proj", 34);
    assert_eq!(line.spans.len(), 3, "no path span: {:?}", line.spans);
}

#[test]
fn fmt_ctx_formats_grades_and_hides_until_first_usage() {
    // No Usage event yet → no segment at all.
    assert!(fmt_ctx(0, 256_000).is_none());
    // Unknown window size → absolute value only, dim.
    let (t, s) = fmt_ctx(54_700, 0).unwrap();
    assert_eq!(t, "ctx 54.7k");
    assert_eq!(s.fg, Some(dim_fg()));
    // Known window → percentage + both numbers; round denominator drops
    // its ".0". Fill-level color grading: dim < 60%, yellow ≥ 60%, red ≥ 80%.
    let (t, s) = fmt_ctx(54_700, 256_000).unwrap();
    assert_eq!(t, "ctx 21% (54.7k/256k)");
    assert_eq!(s.fg, Some(dim_fg()));
    let (_, s) = fmt_ctx(160_000, 256_000).unwrap(); // 62%
    assert_eq!(s.fg, Some(TOOL_CALL_FG));
    let (t, s) = fmt_ctx(210_000, 256_000).unwrap(); // 82%
    assert_eq!(t, "ctx 82% (210.0k/256k)");
    assert_eq!(s.fg, Some(TOOL_ERR_FG));
}

#[test]
fn chrome_fg_is_one_busy_color_for_chrome_and_status() {
    // Busy: the whole bottom chrome (input border + status state) reads as
    // one AGENT_FG unit. Idle: the USER_FG idle color.
    assert_eq!(chrome_fg(true), AGENT_FG);
    assert_eq!(chrome_fg(false), USER_FG);
}

#[test]
fn slash_help_reply_is_a_multiline_list() {
    // Minimal config — the Help arm doesn't read it, but slash_reply's
    // signature needs one.
    let cfg: Config =
        serde_json::from_str(r#"{"provider":"anthropic","api_key_env":"K","model":"m"}"#).unwrap();
    let help = slash_reply(&SlashCmd::Help, &cfg);
    assert!(
        help.lines().count() > 3,
        "help should be multiline: {help:?}"
    );
    for cmd in [
        "/clear", "/export", "/model", "/limits", "/tools", "/system", "/context", "/compact",
        "/help",
    ] {
        assert!(
            help.lines().any(|l| l.contains(cmd)),
            "missing {cmd}: {help:?}"
        );
    }
    // Indented one-command-per-line layout.
    assert!(help.lines().any(|l| l.starts_with("  /export")));
}

#[test]
fn slash_model_reply_redacts_url_userinfo() {
    // Round-2 review (R2-IMP-1 promoted): `/model` echoes base_url — a
    // `https://user:pass@host` endpoint must not put the password on screen.
    // Same credential-display discipline as every other URL surface.
    let cfg: Config = serde_json::from_str(
        r#"{"provider":"anthropic","api_key_env":"K","model":"m",
            "base_url":"https://alice:s3cret@example.com/v1"}"#,
    )
    .unwrap();
    let reply = slash_reply(&SlashCmd::Model, &cfg);
    assert!(
        reply.contains("https://***@example.com/v1"),
        "userinfo must be masked: {reply:?}"
    );
    assert!(
        !reply.contains("alice") && !reply.contains("s3cret"),
        "credential fragments must not survive: {reply:?}"
    );
}

/// Hand-built breakdown for the render tests — numbers chosen so the
/// sections, bars, percentages and the window line all have something
/// to show.
fn sample_breakdown() -> crate::context_report::ContextBreakdown {
    use crate::context_report as cr;
    let tool = |name: &str, tokens: u64| cr::ToolEntry {
        name: name.into(),
        tokens,
    };
    cr::ContextBreakdown {
        system: cr::SystemBreakdown {
            env: 122,
            base: 898,
            skill_intro: 214,
            tokens: 1234,
        },
        tools: cr::ToolsBreakdown {
            builtin: cr::ToolCategory {
                tokens: 1800,
                tools: vec![
                    tool("shell", 620),
                    tool("read_file", 540),
                    tool("task", 352),
                    tool("todo", 288),
                ],
            },
            mcp: cr::ToolCategory {
                tokens: 500,
                tools: vec![tool("mcp_fetch", 500)],
            },
            skill: cr::ToolCategory {
                tokens: 200,
                tools: vec![tool("read_skill", 120), tool("list_skill", 80)],
            },
            custom: cr::ToolCategory::default(),
            tokens: 2500,
        },
        messages: cr::MessagesBreakdown {
            tokens: 8611,
            by_kind: cr::MessageKinds {
                reasoning: 0,
                user_text: 1200,
                assistant_text: 1000,
                tool_use: 411,
                tool_result: 5000,
                image: 1000,
                document: 0,
                skill_content: 0,
                other: 0,
            },
        },
        total_tokens: 12345,
        calibration: None,
    }
}

#[test]
fn render_context_report_calibration_and_window_states() {
    let mut bd = sample_breakdown();
    let s = render_context_report(&bd, 0);
    assert!(
        s.contains("uncalibrated heuristic (chars/4; no measurement yet)"),
        "header: {s:?}"
    );
    assert!(
        s.contains("window: no max_context_tokens configured"),
        "window line omitted: {s:?}"
    );
    bd.calibration = Some(crate::context_report::Calibration {
        measured_prompt_tokens: 10_000,
        scale: 1.83,
    });
    let s = render_context_report(&bd, 0);
    assert!(s.contains("calibrated ×1.83"), "header: {s:?}");
    assert!(s.contains("10.0k tok)"), "anchor size shown: {s:?}");
}

/// Carried reasoning gets its own line only when there is some, and the
/// unset-window line says how to fix it (a session that carries no reasoning
/// sees a report unchanged from before the bucket existed).
#[test]
fn render_context_report_lists_carried_reasoning_and_explains_the_unset_window() {
    let mut bd = sample_breakdown();
    let plain = render_context_report(&bd, 0);
    assert!(!plain.contains("reasoning"), "sparse: {plain:?}");
    assert!(
        plain.contains("window: no max_context_tokens configured — set it to the model's context"),
        "the unset window says how to fix it: {plain:?}"
    );
    bd.messages.by_kind.reasoning = 887;
    let s = render_context_report(&bd, 0);
    assert!(
        s.contains("reasoning"),
        "carried reasoning is listed: {s:?}"
    );
    // After "skill content" and before the window line — the kind rows keep
    // their order, the new one joins the tail.
    let at = s.find("    reasoning").expect("its own row");
    assert!(s[..at].contains("skill content"), "{s:?}");
    assert!(s[at..].contains("window: "), "{s:?}");
}

#[test]
fn render_context_report_sections_tools_and_window() {
    let bd = sample_breakdown();
    let s = render_context_report(&bd, 200_000);
    for label in [
        "system",
        "env header",
        "base",
        "skill intro",
        "tools",
        "builtin",
        "mcp",
        "skill",
        "custom",
        "messages",
        "user text",
        "tool results",
        "images",
        "skill content",
    ] {
        assert!(s.contains(label), "missing {label}:\n{s}");
    }
    // Per-tool lines are present, biggest-first within the category.
    let shell = s.lines().find(|l| l.contains("shell")).expect("shell line");
    assert!(shell.contains("620"), "shell tokens: {shell}");
    // window: 12345/200000 → 6% used, free ≈ 187.7k.
    assert!(s.contains("window: 6% of 200k max_context_tokens"), "{s}");
    assert!(s.contains("(187.7k free)"), "free value shown: {s}");
}

#[test]
fn render_context_report_columns_align() {
    // The pre-fix {:<10}/{:<16} pads let the longest standing labels
    // ("skill content" = 13) and long MCP names push the tokens column
    // out of alignment while every label still passed a contains()
    // check. Pin the layout: for every data row the column just past
    // the label/name field is the separator space, and the 7-wide
    // tokens field is flush against its right edge. Char-based, since
    // the truncation ellipsis is multi-byte.
    use crate::context_report as cr;
    let mut bd = sample_breakdown();
    bd.tools.mcp.tools.push(cr::ToolEntry {
        name: "mcp_github_create_issue".into(),
        tokens: 300,
    });
    bd.messages.by_kind.skill_content = 77; // the 13-char label row
    let s = render_context_report(&bd, 0);
    let mut in_advisory = false;
    for line in s.lines() {
        let indent = line.len() - line.trim_start_matches(' ').len();
        let bare = line.trim_start_matches(' ');
        if bare.starts_with("advisory:") {
            in_advisory = true;
        }
        if in_advisory && indent > 2 {
            continue; // advisory prose (first row + guidance): no columns
        }
        in_advisory = false;
        if indent == 0 || bare.starts_with('…') {
            continue; // header / window / overflow: no columns
        }
        let (col, kind) = match indent {
            2 => (13, "section"),
            4 => (13, "sub"),
            6 => (24, "tool"),
            _ => panic!("unexpected indent {indent}: {line:?}"),
        };
        let at = |i: usize| line.chars().nth(i).unwrap_or(' ');
        assert_eq!(
            at(indent + col),
            ' ',
            "{kind} separator column spilled into: {line:?}"
        );
        assert_ne!(
            at(indent + col + 7),
            ' ',
            "{kind} tokens not flush right: {line:?}"
        );
    }
    // The 23-char name fits; a longer one truncates with "…" to exactly
    // the column width instead of drifting.
    assert!(s.contains("mcp_github_create_issue"), "{s}");
    let mut long = bd;
    long.tools.mcp.tools[0].name = "mcp_github_create_pull_request_review_comment".into();
    let s = render_context_report(&long, 0);
    let row = s
        .lines()
        .find(|l| l.contains("mcp_github_create"))
        .expect("truncated row present");
    assert!(row.contains('…'), "truncated: {row}");
    assert_eq!(row.trim_end().chars().count(), 6 + 24 + 1 + 7);
}

#[test]
fn render_context_report_lists_top_five_then_overflow() {
    // The per-category cap lists the five biggest and counts the rest —
    // the "… +N more" branch had no coverage before.
    use crate::context_report as cr;
    let mut bd = sample_breakdown();
    for i in 0..3 {
        bd.tools.builtin.tools.push(cr::ToolEntry {
            name: format!("extra_{i}"),
            tokens: 10 + i,
        });
    }
    let s = render_context_report(&bd, 0);
    assert!(s.contains("… +2 more"), "overflow count: {s}");
    assert!(s.contains("extra_2"), "5th largest listed: {s}");
    assert!(!s.contains("extra_0"), "7th largest hidden: {s}");
}

/// The tool-table advisory: a short block under the tool categories when the
/// table is over one of the kernel's lines, naming the line actually crossed;
/// absent under both — and exactly ON a line counts as under.
#[test]
fn render_context_report_advises_an_oversized_tool_table() {
    use crate::context_report as cr;
    // Under both lines (2500 tokens / 7 tools): no block at all.
    let plain = render_context_report(&sample_breakdown(), 0);
    assert!(!plain.contains("advisory"), "{plain}");

    // Over on count alone: replacing the one MCP tool by 25 makes 31.
    let mut many = sample_breakdown();
    many.tools.mcp.tools = (0..25)
        .map(|i| cr::ToolEntry {
            name: format!("mcp_{i}"),
            tokens: 20,
        })
        .collect();
    let s = render_context_report(&many, 0);
    let at = s.find("    advisory: ").expect("the advisory block");
    // The kernel's own lines (the same text the construction-time notice
    // prints), one per row: numbers, the line crossed, then the guidance.
    let block: Vec<&str> = s[at..].lines().take(5).collect();
    assert_eq!(
        block,
        [
            "    advisory: tool definitions ≈ 2500 tokens across 31 tools.",
            "      Over the advisory line of 30 tools.",
            "      Fixed cost on every request; compaction cannot fold it.",
            "      Trim tools / MCP servers, or move rarely used tools into skills.",
            "      Rules of thumb from Anthropic's Tool search docs, on estimates.",
        ]
    );
    // Only the crossed line is cited.
    assert!(
        !s[at..].contains("10000"),
        "the token line is not crossed: {s}"
    );
    // Placed after the last tool category and before the messages section.
    assert!(s[..at].contains("custom"), "{s}");
    assert!(
        s[at..].contains("on estimates.\n  messages"),
        "the block ends where the messages section starts: {s}"
    );

    // Over on tokens alone: the token line, not the count line.
    let mut wide = sample_breakdown();
    wide.tools.tokens = 10_001;
    let s = render_context_report(&wide, 0);
    let at = s.find("    advisory: ").expect("the advisory block");
    assert!(
        s[at..].starts_with(concat!(
            "    advisory: tool definitions ≈ 10001 tokens across 7 tools.\n",
            "      Over the advisory line of 10000 tokens.\n",
        )),
        "{s}"
    );

    // Exactly on both lines is not over either.
    let mut on = sample_breakdown();
    on.tools.tokens = 10_000;
    on.tools.mcp.tools = (0..24)
        .map(|i| cr::ToolEntry {
            name: format!("mcp_{i}"),
            tokens: 20,
        })
        .collect();
    assert_eq!(on.tools.tool_count(), 30);
    let s = render_context_report(&on, 0);
    assert!(!s.contains("advisory"), "{s}");
}

#[test]
fn slash_context_rejects_a_stray_arg() {
    assert_eq!(
        classify("/context now"),
        SlashCmd::Unknown("context now".into())
    );
}

// ── Slash-command completion popup ─────────────────────────────────

#[test]
fn slash_popup_opens_filters_and_closes_on_space_or_no_match() {
    let mut p = SlashPopup::new();
    p.update("");
    assert!(!p.is_open(), "empty input");
    p.update("abc");
    assert!(!p.is_open(), "no leading slash");
    // Bare "/" lists every command.
    p.update("/");
    assert!(p.is_open());
    assert_eq!(p.matches.len(), SLASH_COMMANDS.len());
    // Prefix filtering is case-sensitive, like classify.
    p.update("/ex");
    assert_eq!(p.matches.len(), 1);
    assert_eq!(p.matches[0].name, "export");
    p.update("/EX");
    assert!(!p.is_open(), "uppercase must match nothing");
    p.update("/zzz");
    assert!(!p.is_open(), "no match");
    // A space means the argument stage: closed even with a valid name.
    p.update("/export ");
    assert!(!p.is_open(), "argument stage closes the popup");
    // Backspacing out of the argument stage reopens it.
    p.update("/expor");
    assert!(p.is_open());
    assert_eq!(p.matches[0].name, "export");
}

#[test]
fn slash_popup_completion_text_and_selection_cycle() {
    let mut p = SlashPopup::new();
    p.update("/");
    // First match is clear (table order); a no-arg command completes bare.
    assert_eq!(p.completion().as_deref(), Some("/clear"));
    // An argument-taking command (export, compact): trailing space included.
    p.next();
    assert_eq!(p.completion().as_deref(), Some("/export "));
    // Down wraps past the last match back to the first; Up wraps back.
    for _ in 0..SLASH_COMMANDS.len() - 1 {
        p.next();
    }
    assert_eq!(p.sel, 0, "wrap to top");
    p.prev();
    assert_eq!(p.sel, SLASH_COMMANDS.len() - 1, "wrap to bottom");
    assert_eq!(p.completion().as_deref(), Some("/quit"));
    // Editing the filter resets the selection to the top.
    p.update("/s");
    assert_eq!(p.sel, 0);
    assert_eq!(p.matches[0].name, "system");
}

#[test]
fn slash_popup_dismiss_holds_until_the_text_changes() {
    let mut p = SlashPopup::new();
    p.update("/");
    assert!(p.is_open());
    p.dismiss("/");
    assert!(!p.is_open());
    // Same text next frame: stays closed (Esc would never close otherwise).
    p.update("/");
    assert!(!p.is_open(), "dismissed text must not reopen");
    // Any further edit lifts the dismissal.
    p.update("/c");
    assert!(p.is_open());
    // The completion path: buffer becomes "/clear", popup stays closed…
    p.update("/cle");
    let c = p.completion().expect("a match");
    p.dismiss(&c);
    p.update(&c);
    assert!(!p.is_open(), "just-completed text stays closed");
    // …and deleting a char reopens with the shorter prefix.
    p.update("/clea");
    assert!(p.is_open());
}

#[test]
fn popup_key_intercepts_only_plain_keys_while_open() {
    let mut p = SlashPopup::new();
    p.update("/");
    // Up/Down move the selection and consume the key.
    assert!(matches!(
        popup_key(&mut p, KeyCode::Down, KeyModifiers::NONE),
        Some(PopupAction::Consumed)
    ));
    assert_eq!(p.sel, 1);
    assert!(matches!(
        popup_key(&mut p, KeyCode::Up, KeyModifiers::NONE),
        Some(PopupAction::Consumed)
    ));
    assert_eq!(p.sel, 0);
    // Tab and Enter both complete (never send); Esc only dismisses.
    assert!(matches!(
        popup_key(&mut p, KeyCode::Tab, KeyModifiers::NONE),
        Some(PopupAction::Complete(ref c)) if c == "/clear"
    ));
    assert!(matches!(
        popup_key(&mut p, KeyCode::Enter, KeyModifiers::NONE),
        Some(PopupAction::Complete(ref c)) if c == "/clear"
    ));
    assert!(matches!(
        popup_key(&mut p, KeyCode::Esc, KeyModifiers::NONE),
        Some(PopupAction::Dismiss)
    ));
    // Typing and modifier chords are NOT intercepted (Alt+Up = history).
    assert!(popup_key(&mut p, KeyCode::Char('x'), KeyModifiers::NONE).is_none());
    assert!(popup_key(&mut p, KeyCode::Up, KeyModifiers::ALT).is_none());
    assert!(matches!(
        popup_key(&mut p, KeyCode::Tab, KeyModifiers::SHIFT),
        Some(PopupAction::Consumed)
    ));
}

#[test]
fn visible_window_keeps_the_selection_inside() {
    // Under the cap everything shows.
    assert_eq!(visible_window(5, 4, POPUP_MAX_ROWS), (0, 5));
    // Over the cap the window slides to keep the selection visible.
    assert_eq!(visible_window(12, 0, POPUP_MAX_ROWS), (0, 8));
    assert_eq!(visible_window(12, 7, POPUP_MAX_ROWS), (0, 8));
    assert_eq!(visible_window(12, 8, POPUP_MAX_ROWS), (1, 8));
    assert_eq!(visible_window(12, 11, POPUP_MAX_ROWS), (4, 8));
}

#[test]
fn slash_popup_lines_format_highlight_and_pad_the_selection() {
    let mut p = SlashPopup::new();
    p.update("/");
    let lines = slash_popup_lines(&p, POPUP_MAX_ROWS);
    // With 9 commands over the 8-row cap, the bare "/" list is windowed
    // (selection 0 → the window starts at the top).
    assert_eq!(lines.len(), POPUP_MAX_ROWS.min(SLASH_COMMANDS.len()));
    // Row format: gutter + "/name" + optional " [arg]" + description.
    assert_eq!(
        row_text(&lines[1]),
        "   /export [path]  dump the transcript to a file (or clipboard)"
    );
    assert_eq!(
        row_text(&lines[0]).trim(),
        "❯ /clear  clear the conversation history"
    );
    // The selected row uses the shared raised surface and is padded to the
    // content width; the others stay on the terminal background.
    let content_w = lines.iter().map(|l| l.width()).max().unwrap();
    assert_eq!(lines[0].style.bg, Some(keycap_bg()));
    assert_eq!(lines[0].width(), content_w);
    assert_eq!(lines[1].style.bg, None);
    // Moving the selection moves the highlight.
    p.next();
    let lines = slash_popup_lines(&p, POPUP_MAX_ROWS);
    assert_eq!(lines[0].style.bg, None);
    assert_eq!(lines[1].style.bg, Some(keycap_bg()));
}

#[test]
fn append_delta_never_drops_text_when_tail_is_not_assistant() {
    // P1: a ghost / notice / tool row pushed mid-stream used to swallow
    // every following delta. Each tail shape must open a FRESH Assistant
    // row and leave the original row untouched.
    for tail in [
        Row::Thinking {
            text: "trace".into(),
            tight: false,
        },
        Row::sys_info("notice"),
        tool_call_row("shell"),
    ] {
        let mut rows = vec![Row::Assistant("done".into()), tail];
        let mut streaming = true; // stream was open when the row landed
        append_delta(&mut rows, &mut streaming, "more");
        assert_eq!(rows.len(), 3);
        assert!(
            matches!(&rows[2], Row::Assistant(t) if t == "more"),
            "delta lost or merged into the interleaved tail row"
        );
        // The interleaved row kept its content.
        assert!(matches!(
            &rows[1],
            Row::Thinking { .. } | Row::System { .. } | Row::ToolCall { .. }
        ));
    }
    // Normal case: tail IS the open Assistant bubble → merge.
    let mut rows = vec![Row::Assistant("a".into())];
    let mut streaming = true;
    append_delta(&mut rows, &mut streaming, "b");
    assert_eq!(rows.len(), 1);
    assert!(matches!(&rows[0], Row::Assistant(t) if t == "ab"));
    // Fresh stream start (streaming=false) opens a row even when the tail
    // is an old Assistant bubble from a previous turn.
    let mut streaming = false;
    append_delta(&mut rows, &mut streaming, "c");
    assert_eq!(rows.len(), 2);
    assert!(matches!(&rows[1], Row::Assistant(t) if t == "c"));
}

#[test]
fn append_thinking_delta_merges_consecutive_and_reopens_after_a_break() {
    // Consecutive parent deltas merge into one trace row.
    let mut rows = vec![];
    let mut ts: Option<String> = None;
    append_thinking_delta(&mut rows, &mut ts, "", "let me ");
    append_thinking_delta(&mut rows, &mut ts, "", "think");
    assert_eq!(rows.len(), 1);
    assert!(matches!(&rows[0], Row::Thinking { text: t, .. } if t == "let me think"));
    assert_eq!(ts.as_deref(), Some(""));

    // The answer starting closes the trace (the event arm resets the
    // flag); a later trace re-opens as a FRESH row instead of merging
    // back into the answer or the old trace.
    ts = None;
    let mut streaming = false;
    append_delta(&mut rows, &mut streaming, "answer");
    append_thinking_delta(&mut rows, &mut ts, "", "more");
    assert_eq!(rows.len(), 3);
    assert!(matches!(&rows[1], Row::Assistant(t) if t == "answer"));
    assert!(matches!(&rows[2], Row::Thinking { text: t, .. } if t == "more"));

    // A tail that isn't the open trace (interleaved notice) opens a new
    // row — deltas are never dropped.
    ts = None;
    rows.push(Row::sys_info("notice"));
    append_thinking_delta(&mut rows, &mut ts, "", "trace");
    assert_eq!(rows.len(), 5);
    assert!(matches!(&rows[4], Row::Thinking { text: t, .. } if t == "trace"));
}

#[test]
fn append_thinking_delta_tags_sub_agent_rows_by_name() {
    let mut rows = vec![];
    let mut ts: Option<String> = None;
    append_thinking_delta(&mut rows, &mut ts, "worker", "planning");
    assert!(matches!(&rows[0], Row::Thinking { text: t, .. } if t == "[worker] planning"));
    // Same child merges; a different child opens its own tagged row.
    append_thinking_delta(&mut rows, &mut ts, "worker", "...");
    assert_eq!(rows.len(), 1);
    ts = None;
    rows.push(Row::sys_info("notice"));
    append_thinking_delta(&mut rows, &mut ts, "other", "hmm");
    assert_eq!(rows.len(), 3);
    assert!(matches!(&rows[2], Row::Thinking { text: t, .. } if t == "[other] hmm"));
}

#[test]
fn last_safe_boundary_only_cuts_at_closed_paragraphs_outside_fences() {
    // A plain paragraph boundary.
    assert_eq!(last_safe_boundary("a\n\nb"), Some(3));
    // Latest boundary wins (para3, not para2).
    let text = "p1\n\np2\n\np3";
    assert_eq!(last_safe_boundary(text), text.find("p3"));
    // A blank line inside an open fence is NOT a boundary; the one after
    // the fence closes is.
    let fenced = "```\ncode\n\nstill code\n```\n\nafter";
    assert_eq!(last_safe_boundary(fenced), fenced.find("after"));
    // No blank line → nothing safe.
    assert_eq!(last_safe_boundary("one\nlong\nparagraph"), None);
    // Leading blanks with no content before them → nothing to commit.
    assert_eq!(last_safe_boundary("\n\nx"), None);
}

#[test]
fn last_safe_boundary_tracks_tilde_fences_and_mixed_markers() {
    // A blank line inside an open ~~~ fence is NOT a boundary (the old
    // flip-on-``` toggle never opened on ~~~ and cut the block in two);
    // the one after the ~~~ closes is.
    let tilde = "~~~\ncode\n\nstill code\n~~~\n\nafter";
    assert_eq!(last_safe_boundary(tilde), tilde.find("after"));
    // ``` and ~~~ never close each other: the foreign fence-looking
    // line inside the block is content, so the blank line after it
    // stays unsafe in BOTH directions.
    let tilde_outer = "~~~\ncode\n```\n\nstill code\n~~~\n\nafter";
    assert_eq!(last_safe_boundary(tilde_outer), tilde_outer.find("after"));
    let backtick_outer = "```\ncode\n~~~\n\nstill code\n```\n\nafter";
    assert_eq!(
        last_safe_boundary(backtick_outer),
        backtick_outer.find("after")
    );
    // Cutting right BEFORE a fence opener is safe (everything before it
    // is closed prose), and blank lines after the ~~~ block closed are
    // boundaries again — tilde fences behave exactly like ``` ones.
    let pre = "para\n\n~~~\ncode\n";
    assert_eq!(last_safe_boundary(pre), pre.find("~~~"));
    let post = "~~~\ncode\n~~~\n\npara2\n\npara3";
    assert_eq!(last_safe_boundary(post), post.find("para3"));
}

#[test]
fn split_streaming_row_flushes_prefix_and_keeps_tail_live() {
    let para = |n: usize| format!("l{n}a\nl{n}b\nl{n}c\nl{n}d"); // 4 lines
    let text = format!("{}\n\n{}\n\n{}", para(1), para(2), para(3)); // 12 lines

    // Above threshold: latest safe boundary = start of para3.
    let mut rows = vec![Row::Assistant(text.clone())];
    split_streaming_row(&mut rows, 80);
    assert_eq!(rows.len(), 2);
    assert!(matches!(&rows[0], Row::Assistant(t) if *t == format!("{}\n\n{}", para(1), para(2))));
    assert!(
        matches!(&rows[1], Row::AssistantCont { text: t, tight: false } if *t == para(3)),
        "paragraph cut keeps the whole para3 as a loose chunk"
    );

    // Below threshold: one row, untouched.
    let mut rows = vec![Row::Assistant(para(1))]; // 4 lines
    split_streaming_row(&mut rows, 80);
    assert_eq!(rows.len(), 1);

    // A huge unclosed paragraph line-cuts instead (own test below).

    // Thinking is deliberately NOT split into committed chunks: the complete
    // trace stays in one row so its final rendering can fold it.
    let original = text.clone();
    let mut rows = vec![Row::Thinking {
        text: original.clone(),
        tight: false,
    }];
    split_streaming_row(&mut rows, 80);
    assert_eq!(rows.len(), 1);
    assert!(matches!(&rows[0], Row::Thinking { text: t, .. } if *t == original));

    // A Cont tail re-splits at the steady-state threshold (already-split →
    // LIVE_TAIL_LINES): the 4-line paragraph no longer fits, so the line
    // cut takes over and the new chunk is tight.
    let mut rows = vec![
        Row::Assistant("head".into()),
        Row::AssistantCont { text, tight: false },
    ];
    split_streaming_row(&mut rows, 80);
    assert_eq!(rows.len(), 3);
    assert!(matches!(&rows[1], Row::AssistantCont { text: t, .. } if t.contains("l1a")));
    assert!(matches!(&rows[2], Row::AssistantCont { text: t, tight: true } if *t == "l3c\nl3d"));

    // Non-streaming tail rows are a no-op.
    let mut rows = vec![tool_call_row("shell")];
    split_streaming_row(&mut rows, 80);
    assert_eq!(rows.len(), 1);
}

#[test]
fn split_deltas_merge_into_the_cont_tail() {
    // After a split, further TextDeltas must append to the Cont tail, not
    // open a new message row.
    let text = format!(
        "{}\n\n{}",
        (0..10)
            .map(|i| format!("x{i}"))
            .collect::<Vec<_>>()
            .join("\n"),
        "tail"
    );
    let mut rows = vec![];
    let mut streaming = false;
    append_delta(&mut rows, &mut streaming, &text);
    split_streaming_row(&mut rows, 80);
    assert_eq!(rows.len(), 2);
    append_delta(&mut rows, &mut streaming, " more");
    assert_eq!(rows.len(), 2, "delta opened a new row instead of merging");
    assert!(
        matches!(&rows[1], Row::AssistantCont { text: t, .. } if t == "tail more"),
        "delta did not land in the tail: {:?}",
        rows[1]
    );
}

#[test]
fn commit_slices_do_not_add_an_end_of_history_blank() {
    // Steady-state chunks: flush against each other, one blank at the
    // true end of history only. A commit SLICE ending mid-history must
    // not carry the trailing blank — every incremental batch would have
    // inserted a spurious empty line into scrollback, tearing the
    // paragraph the tight chunks are meant to keep seamless.
    let rows = vec![
        Row::Assistant("a".into()),
        Row::AssistantCont {
            text: "b".into(),
            tight: true,
        },
        Row::AssistantCont {
            text: "c".into(),
            tight: true,
        },
    ];
    let mut cache = MdCache::new();
    let full = build_history_lines(&rows, &mut cache, TEST_W, LiveRow::None, false, true);
    let texts: Vec<String> = full.iter().map(row_text).collect();
    assert_eq!(
        texts,
        vec![
            "◆ a".to_string(),
            "  b".to_string(),
            "  c".to_string(),
            String::new()
        ]
    );
    // The last chunk alone, rendered as a mid-history commit batch.
    let slice = build_history_lines(&rows[2..], &mut cache, TEST_W, LiveRow::None, false, false);
    let texts: Vec<String> = slice.iter().map(row_text).collect();
    assert_eq!(
        texts,
        vec!["  c".to_string()],
        "no spurious blank: {texts:?}"
    );
}

#[test]
fn est_display_rows_wraps_per_logical_line_with_ceil() {
    // Empty text still occupies one (blank) row — the per-line floor.
    assert_eq!(est_display_rows("", 40), 1);
    // Width 0 is clamped to 1 instead of dividing by zero.
    assert_eq!(est_display_rows("ab", 0), 2);
    // Short ASCII fits one row; the ceil kicks in at the boundary.
    assert_eq!(est_display_rows("hello", 40), 1);
    assert_eq!(est_display_rows("0123456789", 10), 1);
    assert_eq!(est_display_rows("0123456789A", 10), 2);
    // Wide (CJK) chars count 2 display columns each: 12 cols fit one row
    // at width 12 but wrap into 3 rows at width 5.
    assert_eq!(est_display_rows("看来离谱的测", 12), 1);
    assert_eq!(est_display_rows("看来离谱的测", 5), 3);
    // Rows are summed per logical line; an empty line keeps its floor
    // row (a trailing newline leaves an empty last line).
    assert_eq!(est_display_rows("ab\nabcdefgh", 3), 4);
    assert_eq!(est_display_rows("a\n", 40), 2);
    // Zero-width chars (combining marks) add no columns.
    assert_eq!(est_display_rows("e\u{0301}", 1), 1);
}

#[test]
fn soft_cut_offset_keeps_tail_rows_and_tracks_fences() {
    // Too short to cut (and keep=0 is rejected outright).
    assert_eq!(soft_cut_offset("a\nb", 80, 2), None);
    assert_eq!(soft_cut_offset("anything", 80, 0), None);
    // Plain short lines (one display row each): the cut is the start of
    // the last-but-one line.
    assert_eq!(soft_cut_offset("a\nb\nc", 80, 1), Some((4, None)));
    assert_eq!(soft_cut_offset("a\nb\nc\nd", 80, 2), Some((4, None)));
    // Cut inside an open fence (opener before, closer after): the flag
    // carries the marker char AND the opener's length (the tail reopens
    // with a length-true synthetic).
    let fenced = "```\ncode1\ncode2\ncode3";
    assert_eq!(soft_cut_offset(fenced, 80, 1), Some((16, Some(('`', 3)))));
    // Same for a longer opener: the reopen must not shrink it to ```.
    let fenced4 = "````\ncode1\ncode2\ncode3";
    assert_eq!(soft_cut_offset(fenced4, 80, 1), Some((17, Some(('`', 4)))));
    // Cut landing exactly ON the closing fence slides past it: the
    // closer joins the prefix, the tail starts fence-free.
    let closing = "```\nc1\nc2\n```\nz";
    assert_eq!(soft_cut_offset(closing, 80, 2), Some((14, None)));
    // A cut after the fence closed is a plain cut.
    assert_eq!(soft_cut_offset(closing, 80, 1), Some((14, None)));
    // One huge logical line (the common prose shape): the cut sits on a
    // WRAP boundary — 10 chars at width 4 wrap into 3 rows (abcd / efgh /
    // ij); keeping the last row live cuts before "i", keeping two rows
    // cuts before "e".
    assert_eq!(soft_cut_offset("abcdefghij", 4, 1), Some((8, None)));
    assert_eq!(soft_cut_offset("abcdefghij", 4, 2), Some((4, None)));
}

#[test]
fn soft_cut_offset_tracks_tilde_fences_and_mixed_markers() {
    // Cut inside an open ~~~ fence: the fence flag carries the tilde (and
    // its opener length) so the reopen synthesizes ~~~ (not ```).
    let tilde = "~~~\ncode1\ncode2\ncode3";
    assert_eq!(soft_cut_offset(tilde, 80, 1), Some((16, Some(('~', 3)))));
    // A ``` line inside the open ~~~ block is CONTENT, not a closer —
    // the cut (start of "code3") still reports the tilde fence open.
    let mixed = "~~~\ncode1\n```\ncode2\ncode3";
    assert_eq!(soft_cut_offset(mixed, 80, 1), Some((20, Some(('~', 3)))));
    // Mirror image: a ~~~ line inside a ``` block doesn't close it.
    let mixed_rev = "```\ncode1\n~~~\ncode2\ncode3";
    assert_eq!(
        soft_cut_offset(mixed_rev, 80, 1),
        Some((20, Some(('`', 3))))
    );
    // Cut landing exactly ON the ~~~ closer slides past it, fence-free
    // (and a cut after it closed is a plain cut).
    let closing = "~~~\nc1\nc2\n~~~\nz";
    assert_eq!(soft_cut_offset(closing, 80, 2), Some((14, None)));
    assert_eq!(soft_cut_offset(closing, 80, 1), Some((14, None)));
}

#[test]
fn split_streaming_row_row_cuts_long_content() {
    // A paragraph of many short lines, no blank line: no paragraph
    // boundary exists, so the row-level fallback commits everything but
    // the last LIVE_TAIL_ROWS rows — long content must flow up instead
    // of cycling inside the live box until the paragraph closes.
    let lines = (0..12).map(|i| format!("w{i}")).collect::<Vec<_>>();
    let mut rows = vec![Row::Assistant(lines.join("\n"))];
    split_streaming_row(&mut rows, 80);
    assert_eq!(rows.len(), 2);
    assert!(
        matches!(&rows[0], Row::Assistant(t) if *t == lines[..10].join("\n")),
        "prefix commits whole lines: {:?}",
        rows[0]
    );
    assert!(
        matches!(&rows[1], Row::AssistantCont { text: t, tight: true } if *t == "w10\nw11"),
        "tail keeps exactly the last two rows: {:?}",
        rows[1]
    );

    // The common prose shape: ONE huge logical line. `lines().count()`
    // is 1 — a logical-line threshold would never fire; the display-row
    // estimate does (200 chars at width 20 = 10 rows > 8) and cuts on a
    // wrap boundary.
    let mut rows = vec![Row::Assistant("x".repeat(200))];
    split_streaming_row(&mut rows, 20);
    assert_eq!(rows.len(), 2, "one logical line must still split");
    assert!(
        matches!(&rows[0], Row::Assistant(t) if t.len() == 160),
        "prefix keeps the first 8 wrapped rows: {:?}",
        rows[0]
    );
    assert!(
        matches!(&rows[1], Row::AssistantCont { text: t, tight: true } if t.len() == 40),
        "tail keeps the last 2 wrapped rows: {:?}",
        rows[1]
    );

    // Steady state: an already-split row re-cuts at LIVE_TAIL_ROWS, so
    // each further row commits as written.
    let mut rows = vec![
        Row::Assistant("head".into()),
        Row::AssistantCont {
            text: "a\nb\nc".into(),
            tight: true,
        },
    ];
    split_streaming_row(&mut rows, 80);
    assert_eq!(rows.len(), 3);
    assert!(matches!(&rows[1], Row::AssistantCont { text: t, .. } if t == "a"));
    assert!(matches!(&rows[2], Row::AssistantCont { text: t, tight: true } if t == "b\nc"));

    // A young paragraph (≤ LIVE_TAIL_ROWS) still gets its loose chunk —
    // paragraph structure keeps blank-line separators in the flow.
    let mut rows = vec![Row::AssistantCont {
        text: "a\nb\nc\n\nd".into(),
        tight: true,
    }];
    split_streaming_row(&mut rows, 80);
    assert_eq!(rows.len(), 2);
    assert!(matches!(&rows[1], Row::AssistantCont { text: t, tight: false } if t == "d"));
}

#[test]
fn soft_cut_preserves_the_final_assistant_display_rows() {
    fn rendered(rows: &[Row]) -> Vec<String> {
        let mut cache = MdCache::new();
        let lines = build_history_lines(rows, &mut cache, 22, LiveRow::None, false, false);
        wrap_history(&lines, 22).iter().map(row_text).collect()
    }

    let source = "x".repeat(60);
    let direct = rendered(&[Row::AssistantCont {
        text: source.clone(),
        tight: true,
    }]);
    let mut split = vec![Row::AssistantCont {
        text: source,
        tight: true,
    }];
    split_streaming_row(&mut split, 20);
    assert_eq!(split.len(), 2);
    assert_eq!(rendered(&split), direct);
}

#[test]
fn soft_cut_rejects_empty_tails_and_preserves_midline_fence_state() {
    // Sliding past a closing fence at EOF would create an empty live tail.
    assert_eq!(soft_cut_offset("```\n```\n", 80, 2), None);
    // The cut is between "```" and "ab" in one source line. That partial
    // prefix is not a complete closing fence; the tail must re-open the block.
    assert_eq!(
        soft_cut_offset("```\n```ab\nx", 3, 2),
        Some((7, Some(('`', 3))))
    );
}

#[test]
fn soft_cut_waits_instead_of_splitting_inline_code() {
    let text = format!("`{}`", "x".repeat(40));
    assert_eq!(soft_cut_offset(&text, 20, 2), None);
    let mut rows = vec![Row::AssistantCont {
        text: text.clone(),
        tight: true,
    }];
    split_streaming_row(&mut rows, 20);
    assert_eq!(rows.len(), 1);
    assert!(matches!(&rows[0], Row::AssistantCont { text: actual, .. } if *actual == text));
}

#[test]
fn abandoned_streaming_splits_leave_the_source_byte_for_byte_unchanged() {
    for original in ["\n\n", "```\n```\n"] {
        let mut rows = vec![Row::AssistantCont {
            text: original.to_string(),
            tight: true,
        }];
        split_streaming_row(&mut rows, 80);
        assert_eq!(rows.len(), 1, "unexpected split for {original:?}");
        assert!(
            matches!(&rows[0], Row::AssistantCont { text, .. } if text == original),
            "abandoned split mutated {original:?}: {:?}",
            rows[0]
        );
    }
}

#[test]
fn split_streaming_row_reopens_a_cut_fence_in_the_tail() {
    // An unclosed fence longer than the viewport: the line cut lands
    // inside it, so the tail gets a synthetic "```\n" opener — both
    // halves must render as code blocks on their own.
    let code = (0..11).map(|i| format!("c{i}")).collect::<Vec<_>>();
    let text = format!("```\n{}", code.join("\n"));
    let mut rows = vec![Row::Assistant(text)];
    split_streaming_row(&mut rows, 80);
    assert_eq!(rows.len(), 2);
    assert!(
        matches!(&rows[0], Row::Assistant(t) if *t == format!("```\n{}", code[..9].join("\n"))),
        "unclosed prefix keeps its lines: {:?}",
        rows[0]
    );
    assert!(
        matches!(&rows[1], Row::AssistantCont { text: t, .. } if *t == format!("```\n{}\n{}", code[9], code[10])),
        "tail re-opens the fence: {:?}",
        rows[1]
    );
    // Both halves render as code (guided `▎ ` bodies, no blank prose).
    let mut cache = MdCache::new();
    let out = build_history_lines(&rows, &mut cache, TEST_W, LiveRow::None, false, true);
    let joined: String = out.iter().map(row_text).collect::<Vec<_>>().join("\n");
    assert!(
        joined.contains("▎ c8"),
        "prefix renders as code: {joined:?}"
    );
    assert!(joined.contains("▎ c9"), "tail renders as code: {joined:?}");

    // The transcript strips the synthetic opener back out.
    let dumped = render_transcript(&rows);
    assert!(
        !dumped.contains("\n```\nc9"),
        "synthetic opener leaked: {dumped:?}"
    );
    assert!(
        dumped.contains("c9\nc10"),
        "tight join is a single \\n: {dumped:?}"
    );
}

#[test]
fn split_streaming_row_reopens_a_cut_tilde_fence_with_tildes() {
    // The ~~~ counterpart: an unclosed tilde block longer than the
    // viewport must NOT be treated as prose (the old fence toggle only
    // knew ```), and the tail's synthetic opener uses the fence's OWN
    // marker — ``` would open a DIFFERENT fence and leave the prefix's
    // ~~~ unclosed.
    let code = (0..11).map(|i| format!("t{i}")).collect::<Vec<_>>();
    let text = format!("~~~\n{}", code.join("\n"));
    let mut rows = vec![Row::Assistant(text)];
    split_streaming_row(&mut rows, 80);
    assert_eq!(rows.len(), 2);
    assert!(
        matches!(&rows[0], Row::Assistant(t) if *t == format!("~~~\n{}", code[..9].join("\n"))),
        "unclosed tilde prefix keeps its lines: {:?}",
        rows[0]
    );
    assert!(
        matches!(&rows[1], Row::AssistantCont { text: t, .. } if *t == format!("~~~\n{}\n{}", code[9], code[10])),
        "tail re-opens the TILDE fence with tildes: {:?}",
        rows[1]
    );
    // Both halves render as code (guided `▎ ` bodies, no prose render).
    let mut cache = MdCache::new();
    let out = build_history_lines(&rows, &mut cache, TEST_W, LiveRow::None, false, true);
    let joined: String = out.iter().map(row_text).collect::<Vec<_>>().join("\n");
    assert!(
        joined.contains("▎ t8"),
        "tilde prefix renders as code: {joined:?}"
    );
    assert!(
        joined.contains("▎ t9"),
        "reopened tilde tail renders as code: {joined:?}"
    );
    // The transcript strips the synthetic ~~~ opener back out.
    let dumped = render_transcript(&rows);
    assert!(
        !dumped.contains("\n~~~\nt9"),
        "synthetic tilde opener leaked: {dumped:?}"
    );
    assert!(
        dumped.contains("t9\nt10"),
        "tight join is a single \\n: {dumped:?}"
    );
}

#[test]
fn tight_chunks_render_flush_and_thinking_keeps_one_marker() {
    // Mid-paragraph chunks: no blank line between them and the row they
    // continue — a gap would tear one paragraph into two blocks.
    let rows = vec![
        Row::Assistant("a".into()),
        Row::AssistantCont {
            text: "b".into(),
            tight: true,
        },
    ];
    let texts: Vec<String> = history(&rows).iter().map(row_text).collect();
    assert_eq!(
        texts,
        vec!["◆ a".to_string(), "  b".to_string(), String::new()]
    );

    // Thinking no longer produces tight split chunks; independently supplied
    // adjacent traces retain their own markers rather than masquerading as a
    // continuation with no header.
    let rows = vec![
        Row::Thinking {
            text: "t1".into(),
            tight: false,
        },
        Row::Thinking {
            text: "t2".into(),
            tight: true,
        },
    ];
    let texts: Vec<String> = history(&rows).iter().map(row_text).collect();
    assert_eq!(
        texts,
        vec!["✻ t1".to_string(), "✻ t2".to_string(), String::new()]
    );

    // Loose thinking chunks keep the old shape: marker + blank between.
    let rows = vec![
        Row::Thinking {
            text: "t1".into(),
            tight: false,
        },
        Row::Thinking {
            text: "t2".into(),
            tight: false,
        },
    ];
    let texts: Vec<String> = history(&rows).iter().map(row_text).collect();
    assert_eq!(
        texts,
        vec![
            "✻ t1".to_string(),
            String::new(),
            "✻ t2".to_string(),
            String::new()
        ]
    );

    // Transcript: tight joins with a single \n, loose with \n\n.
    let rows = vec![
        Row::Assistant("first".into()),
        Row::AssistantCont {
            text: "second".into(),
            tight: true,
        },
    ];
    let out = render_transcript(&rows);
    assert!(out.contains("Tiny\nfirst\nsecond"), "tight join: {out:?}");
    assert!(
        !out.contains("first\n\nsecond"),
        "tight must not gap: {out:?}"
    );
}

#[test]
fn append_sub_delta_merges_by_name_and_recovers_after_interruption() {
    let mut rows = vec![];
    let mut sub: Option<String> = None;
    append_sub_delta(&mut rows, &mut sub, "a", "hello");
    append_sub_delta(&mut rows, &mut sub, "a", " world");
    assert_eq!(rows.len(), 1);
    assert!(matches!(&rows[0], Row::Assistant(t) if t == "[a] hello world"));
    // A different sub-agent starts its own bubble.
    append_sub_delta(&mut rows, &mut sub, "b", "other");
    assert_eq!(rows.len(), 2);
    assert!(matches!(&rows[1], Row::Assistant(t) if t == "[b] other"));
    // Same sub-agent again but the tail is now a System notice → fresh
    // bubble with the prefix re-stated (the old code dropped this delta).
    rows.push(Row::sys_warn("[warn] x"));
    append_sub_delta(&mut rows, &mut sub, "b", "resumed");
    assert_eq!(rows.len(), 4);
    assert!(matches!(&rows[3], Row::Assistant(t) if t == "[b] resumed"));
}

#[test]
fn strip_controls_removes_esc_csi_and_nul_keeps_newline_tab() {
    // A full ANSI color sequence: ESC and the CSI bracket codes' ESC go,
    // leaving only printable residue.
    assert_eq!(strip_controls("\x1b[31mred\x1b[0m"), "[31mred[0m");
    assert_eq!(strip_controls("a\x00b\x07c"), "abc");
    assert_eq!(strip_controls("line1\nline2\there"), "line1\nline2\there");
    // Plain text is untouched (and the common case allocates once).
    assert_eq!(strip_controls("plain"), "plain");
}

#[test]
fn strip_controls_json_sanitizes_nested_strings() {
    let v = serde_json::json!({
        "cmd": "echo \x1b[1mhi",
        "args": ["a\x00b", {"nested": "x\x07y"}],
        "count": 3,
    });
    let clean = strip_controls_json(&v);
    assert_eq!(clean["cmd"], "echo [1mhi");
    assert_eq!(clean["args"][0], "ab");
    assert_eq!(clean["args"][1]["nested"], "xy");
    assert_eq!(clean["count"], 3); // non-strings pass through
}

// ── ANSI parsing (ansi_to_lines / ansi_guided_lines / ansi_plain) ───

#[test]
fn ansi_to_lines_colors_a_git_diff_and_splits_at_style_changes() {
    // The git-diff case: red removals, green additions, default context.
    let lines = ansi_to_lines("\x1b[31m- old\x1b[0m\n\x1b[32m+ new\x1b[0m\n plain");
    assert_eq!(lines.len(), 3);
    assert_eq!(row_text(&lines[0]), "- old");
    assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
    assert_eq!(row_text(&lines[1]), "+ new");
    assert_eq!(lines[1].spans[0].style.fg, Some(Color::Green));
    // After the reset the context line is back to the default style.
    assert_eq!(row_text(&lines[2]), " plain");
    assert_eq!(lines[2].spans[0].style, Style::default());
    // A mid-line SGR change splits spans: red head, default tail — not one
    // span carrying whichever style happened to be last.
    let lines = ansi_to_lines("\x1b[31mred\x1b[0mplain");
    assert_eq!(lines[0].spans.len(), 2);
    assert_eq!(lines[0].spans[0].content.as_ref(), "red");
    assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
    assert_eq!(lines[0].spans[1].content.as_ref(), "plain");
    assert_eq!(lines[0].spans[1].style, Style::default());
}

#[test]
fn ansi_to_lines_handles_modifiers_bg_and_channel_resets() {
    // Bold + standard fg.
    let lines = ansi_to_lines("\x1b[1;33mbold yellow\x1b[0m");
    let st = lines[0].spans[0].style;
    assert_eq!(st.fg, Some(Color::Yellow));
    assert!(st.add_modifier.contains(Modifier::BOLD));
    // Bright fg (91) + standard bg (44).
    let lines = ansi_to_lines("\x1b[91;44mhi\x1b[0m");
    let st = lines[0].spans[0].style;
    assert_eq!(st.fg, Some(Color::LightRed));
    assert_eq!(st.bg, Some(Color::Blue));
    // 39 resets only fg; indexed 256-color SGR keeps its exact palette index.
    let lines = ansi_to_lines("\x1b[31;42mx\x1b[39my\x1b[38;5;196mz");
    assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
    assert_eq!(lines[0].spans[1].style.fg, None);
    assert_eq!(lines[0].spans[1].style.bg, Some(Color::Green));
    assert_eq!(lines[0].spans[2].style.fg, Some(Color::Indexed(196)));
}

#[test]
fn ansi_to_lines_strips_osc_and_truncated_sequences() {
    // OSC hyperlink (BEL- and ESC\-terminated forms) stripped, text kept.
    let lines = ansi_to_lines("\x1b]8;;http://x\x07link\x1b]8;;\x1b\\ done");
    assert_eq!(row_text(&lines[0]), "link done");
    // A CSI cut off at end-of-input is dropped without panicking.
    let lines = ansi_to_lines("\x1b[31mabc\x1b[");
    assert_eq!(row_text(&lines[0]), "abc");
    // Non-m CSI (cursor moves) and stray control chars are stripped.
    let lines = ansi_to_lines("a\x1b[2Jb\x00c\x07d");
    assert_eq!(row_text(&lines[0]), "abcd");
    // Empty input still yields one (empty) line.
    assert_eq!(ansi_to_lines("").len(), 1);
}

#[test]
fn ansi_guided_lines_guides_and_falls_back_to_base_style() {
    let err_style = Style::default().fg(TOOL_ERR_FG);
    let lines = ansi_guided_lines("\x1b[31m- old\x1b[0m\nplain", err_style);
    // Every line gets the dim "│ " guide.
    for l in &lines {
        assert_eq!(l.spans[0].content.as_ref(), "│ ");
        assert_eq!(l.spans[0].style.fg, Some(dim_fg()));
    }
    // ANSI-colored spans keep their own color…
    let red = lines[0]
        .spans
        .iter()
        .find(|s| s.content.as_ref() == "- old")
        .expect("the red span");
    assert_eq!(red.style.fg, Some(Color::Red));
    // …unstyled spans fall back to the base style (error red here).
    assert_eq!(lines[1].spans[1].content.as_ref(), "plain");
    assert_eq!(lines[1].spans[1].style, err_style);
}

#[test]
fn ansi_plain_strips_codes_keeps_text() {
    assert_eq!(ansi_plain("\x1b[31m- old\x1b[0m\nplain"), "- old\nplain");
    assert_eq!(ansi_plain("no codes"), "no codes");
}

#[test]
fn ansi_plain_keeps_the_trailing_blank_line_a_newline_opens() {
    // "a\n" shows one more (empty) line than "a" on screen; the tail
    // flush must materialize it or ansi_plain's join drops it and
    // /export ends one line short of the display.
    assert_eq!(ansi_plain("a\n"), "a\n");
    assert_eq!(ansi_plain("a"), "a");
    let lines = ansi_to_lines("a\n");
    assert_eq!(lines.len(), 2);
    assert_eq!(row_text(&lines[1]), "");
    // No trailing newline → the line count is unchanged.
    assert_eq!(ansi_to_lines("a").len(), 1);
    // Every newline opens a line: two newlines, two blank tails.
    assert_eq!(ansi_plain("a\n\n"), "a\n\n");
}

#[test]
fn ansi_to_lines_bare_esc_does_not_eat_its_neighbor() {
    // A lone ESC followed by '\n': the newline is plain text, not a
    // sequence introducer — it must survive (two lines), not be
    // swallowed as the second byte of a two-char escape.
    let lines = ansi_to_lines("a\x1b\nb");
    assert_eq!(lines.len(), 2);
    assert_eq!(row_text(&lines[0]), "a");
    assert_eq!(row_text(&lines[1]), "b");
    // An ordinary letter after ESC is not an introducer either: the ESC
    // is dropped, 'X' stays.
    assert_eq!(ansi_to_lines("\x1bXb").len(), 1);
    assert_eq!(row_text(&ansi_to_lines("\x1bXb")[0]), "Xb");
    // Real introducers still strip: charset selection (ESC ( B) and the
    // standalone finals (ESC c) keep only their printable tail.
    assert_eq!(row_text(&ansi_to_lines("\x1b(Bb")[0]), "Bb");
    assert_eq!(row_text(&ansi_to_lines("\x1bcx")[0]), "x");
}

#[test]
fn ansi_tabs_expand_to_two_spaces_on_both_paths() {
    // unicode-width counts \t as zero columns, which would wrap tabbed
    // tool output wrong; both the render path (ansi_to_lines) and the
    // export/preview path (ansi_plain) expand it the same way.
    let lines = ansi_to_lines("a\tb");
    assert_eq!(row_text(&lines[0]), "a  b");
    assert_eq!(ansi_plain("a\tb"), "a  b");
}

#[test]
fn flush_next_queued_sends_fifo_and_appends_only_real_user_rows() {
    let (tx, rx) = mpsc::channel::<Option<Input>>();
    let mut queued = std::collections::VecDeque::from(vec!["q1".to_string(), "q2".to_string()]);
    let mut rows = vec![Row::Assistant("run output".into())];

    let out = flush_next_queued(&mut queued, &mut rows, &tx);
    assert_eq!(out, Flush::Sent);
    assert!(matches!(rx.try_recv(), Ok(Some(Input::Text(t))) if t == "q1"));
    assert_eq!(queued, std::collections::VecDeque::from(["q2".to_string()]));
    assert!(matches!(rows.last(), Some(Row::User(t)) if t == "q1"));

    let out = flush_next_queued(&mut queued, &mut rows, &tx);
    assert_eq!(out, Flush::Sent);
    assert!(matches!(rx.try_recv(), Ok(Some(Input::Text(t))) if t == "q2"));
    assert!(queued.is_empty());

    let before = rows.len();
    let out = flush_next_queued(&mut queued, &mut rows, &tx);
    assert_eq!(out, Flush::Empty);
    assert_eq!(rows.len(), before);

    queued.push_back("q3".to_string());
    drop(rx);
    let out = flush_next_queued(&mut queued, &mut rows, &tx);
    assert_eq!(out, Flush::Undeliverable);
    assert!(rows.iter().any(|r| matches!(r, Row::User(t) if t == "q3")));
    assert!(rows.iter().any(|r| matches!(
        r,
        Row::System {
            level: SysLevel::Error,
            ..
        }
    )));
}

#[test]
fn apply_flush_sent_starts_a_run_and_idle_flushes_clear_state() {
    let mut phase = RunPhase::Failed;
    let mut run_start = None;
    let mut activity = String::new();
    let mut thinking = false;
    apply_flush(
        Flush::Sent,
        &mut phase,
        &mut run_start,
        &mut activity,
        &mut thinking,
    );
    assert_eq!(phase, RunPhase::Running);
    assert!(run_start.is_some());
    assert_eq!(activity, "thinking");
    assert!(thinking);
    let first_start = run_start.unwrap();

    apply_flush(
        Flush::Sent,
        &mut phase,
        &mut run_start,
        &mut activity,
        &mut thinking,
    );
    assert!(run_start.unwrap() >= first_start);

    apply_flush(
        Flush::Empty,
        &mut phase,
        &mut run_start,
        &mut activity,
        &mut thinking,
    );
    assert_eq!(phase, RunPhase::Idle);
    assert!(run_start.is_none());
    assert!(!thinking);
    assert_eq!(activity, "thinking");

    apply_flush(
        Flush::Undeliverable,
        &mut phase,
        &mut run_start,
        &mut activity,
        &mut thinking,
    );
    assert_eq!(phase, RunPhase::Failed);
    assert!(run_start.is_none());
    assert!(!thinking);
    assert_eq!(activity, "thinking");
}

#[test]
fn commit_pending_end_advances_to_end_with_no_queue_or_stream() {
    let rows = vec![Row::User("hi".into()), Row::Assistant("hey".into())];
    assert_eq!(commit_pending_end(&rows, 0, false), 2);
}

#[test]
fn commit_pending_end_holds_back_a_trailing_streaming_assistant() {
    let rows = vec![Row::User("hi".into()), Row::Assistant("streaming".into())];
    // Stream open: the in-flight Assistant (last row) stays in the live
    // viewport, not committed to scrollback until the stream closes.
    assert_eq!(commit_pending_end(&rows, 0, true), 1);
}

#[test]
fn commit_pending_end_holds_back_a_trailing_thinking_trace() {
    // A reasoning trace streams like the reply does: held in the live
    // viewport while open, committed once closed. When the answer has
    // started, the trace is no longer trailing — it commits right away
    // and only the answer row is held.
    let rows = vec![
        Row::User("hi".into()),
        Row::Thinking {
            text: "trace".into(),
            tight: false,
        },
    ];
    assert_eq!(commit_pending_end(&rows, 0, true), 1);
    assert_eq!(commit_pending_end(&rows, 0, false), 2);
    let rows = vec![
        Row::User("hi".into()),
        Row::Thinking {
            text: "trace".into(),
            tight: false,
        },
        Row::Assistant("answer".into()),
    ];
    assert_eq!(commit_pending_end(&rows, 0, true), 2);
}

#[test]
fn inflight_row_index_covers_a_streaming_trace() {
    // While the trace streams (no Assistant yet), the live zone shows
    // the Thinking row; when the answer streams, it takes over.
    let rows = vec![
        Row::User("hi".into()),
        Row::Thinking {
            text: "trace".into(),
            tight: false,
        },
    ];
    assert_eq!(inflight_row_index(&rows, true), Some(1));
    let rows = vec![
        Row::User("hi".into()),
        Row::Thinking {
            text: "trace".into(),
            tight: false,
        },
        Row::Assistant("answer".into()),
    ];
    assert_eq!(inflight_row_index(&rows, true), Some(2));
}

#[test]
fn inflight_row_index_is_the_last_assistant_while_streaming() {
    let rows = vec![
        Row::User("u".into()),
        Row::Assistant("turn 1".into()),
        Row::sys_warn("warn"),
        Row::Assistant("turn 2".into()),
    ];
    assert_eq!(inflight_row_index(&rows, true), Some(3));
    assert_eq!(inflight_row_index(&rows, false), None);
    assert_eq!(commit_pending_end(&rows, 0, true), 3);
}

#[test]
fn history_expanded_tool_rows_render_full_bodies() {
    // 8a: expanded ToolCall shows the PRETTY JSON (not the collapsed
    // summary), expanded ToolResult the full output — both with guides.
    let call = Row::ToolCall {
        name: "shell".into(),
        input: serde_json::json!({"cmd": "ls", "sudo": true}),
        collapsed: "summary".into(),
        expanded: true,
        started: Instant::now(),
    };
    let result = Row::ToolResult {
        name: "shell".into(),
        content: "line1\nline2".into(),
        collapsed: "line1".into(),
        is_error: false,
        expanded: true,
        elapsed: None,
    };
    let lines = history(&[call, result]);
    let texts: Vec<String> = lines.iter().map(row_text).collect();
    assert!(
        texts.iter().any(|t| t.contains("\"cmd\": \"ls\"")),
        "pretty JSON: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains("summary")),
        "collapsed leaked: {texts:?}"
    );
    assert!(texts.iter().any(|t| t == "    │ line1") && texts.iter().any(|t| t == "    │ line2"));
    // Every body line carries the role gutter followed by the guide span.
    let guided = lines
        .iter()
        .filter(|l| l.spans.get(1).is_some_and(|s| s.content.as_ref() == "│ "));
    assert!(
        guided.count() >= 4,
        "expanded bodies must be guided: {texts:?}"
    );
}

#[test]
fn markdown_unclosed_fence_colors_tail_without_leaking() {
    // 8b: a fence left open at EOF (streamed partial block) must not
    // panic, leak the ``` line, or leave the tail without its border bar.
    let lines = render_markdown("intro\n\n```rust\nlet a = 1;\nlet b = 2;", 40);
    let texts: Vec<String> = lines.iter().map(row_text).collect();
    assert!(
        !texts.iter().any(|t| t.contains("```")),
        "fence leaked: {texts:?}"
    );
    for l in lines.iter().filter(|l| row_text(l).starts_with("▎ let ")) {
        assert_eq!(l.spans[0].style.fg, Some(dim_fg()));
        assert!(
            l.spans[1..].iter().all(|s| s.style.fg.is_none()),
            "unclosed fence tail must stay default fg: {:?}",
            l.spans
        );
    }
}

#[test]
fn build_status_line_width_boundary_keeps_then_drops_stats() {
    let state = "idle · 3s";
    let stats = " · 5 in · 6 out";
    let used = " ◆ glm".width() + format!("  · {state}").width() + stats.width();
    let exact = used;
    // At the exact fit, the full stats are the third span.
    let full = build_status_line("glm", state, false, None, None, stats, "", exact);
    assert_eq!(full.spans[2].content.as_ref(), stats);
    // One column short: stats no longer fit and there's no ctx to fall back
    // to, so the bar is just badge+state.
    let dropped = build_status_line("glm", state, false, None, None, stats, "", exact - 1);
    assert_eq!(dropped.spans.len(), 2);
}

#[test]
fn idle_prompt_glyph_is_two_columns() {
    // The caret math adds prompt.width() to the caret column on the first
    // input line; the stable "❯ " prompt must stay two columns.
    assert_eq!(UnicodeWidthStr::width("❯ "), 2);
}

#[test]
fn markdown_headers_and_lists_restyled() {
    // tui-markdown 0.3 leaves ATX headers literal ("## T", unstyled) and
    // bullets literal ("- x"). render_markdown must fix them while keeping
    // inline styling (code → fg White) on list items.
    let lines = render_markdown("## 标题\n\n- 项目 `code` more\n", 40);
    // Header: a single bold span, markers stripped, just the title.
    let hdr = lines
        .iter()
        .find(|l| {
            l.spans
                .first()
                .is_some_and(|s| s.style.add_modifier.contains(Modifier::BOLD))
        })
        .expect("a bold header line");
    assert_eq!(hdr.spans.len(), 1);
    let ht = hdr.spans[0].content.as_ref();
    assert!(!ht.contains('#'), "header still has '#': {ht}");
    assert_eq!(ht, "标题");

    // List: first span is "• ", and a later span still carries inline code
    // styling from `code` (kept, not dropped by the marker swap).
    let list = lines
        .iter()
        .find(|l| l.spans.first().is_some_and(|s| s.content.as_ref() == "• "))
        .expect("a bulleted list line");
    assert_eq!(list.spans[0].content.as_ref(), "• ");
    assert!(
        list.spans.iter().any(|s| s.style.fg == Some(Color::White)),
        "inline code styling lost: {:?}",
        list.spans
    );
}

#[test]
fn composer_caret_uses_display_width() {
    // Three CJK chars occupy six cells after the two-cell prompt. The shared
    // layout, not InputBuffer's byte offset, determines the terminal cursor.
    let mut b = InputBuffer::new();
    b.set_text("看来离");
    let composer = layout_composer(&b, 20, "❯ ", Style::default());
    assert_eq!((composer.caret_row, composer.caret_col), (0, 8));
}

#[test]
fn type_and_drain() {
    let mut b = InputBuffer::new();
    for c in "hi".chars() {
        b.put_char(c);
    }
    assert_eq!(b.text(), "hi");
    assert_eq!(b.drain_to_string(), "hi");
    assert!(b.is_empty());
}

#[test]
fn insert_str_handles_multiline_crlf_and_cjk() {
    // Paste a multi-line blob at the start: each \n starts a new line.
    let mut b = InputBuffer::new();
    b.insert_str("line1\nline2\nline3");
    assert_eq!(b.text(), "line1\nline2\nline3");
    assert_eq!(b.line_count(), 3);

    // CRLF pastes collapse to a single newline (no stray CR left behind).
    let mut b = InputBuffer::new();
    b.insert_str("a\r\nb");
    assert_eq!(b.text(), "a\nb");

    // A LONE \r survives here as ordinary content (only the CR of a CRLF
    // pair collapses): line-break handling is this helper's whole contract —
    // control-char sanitizing belongs to the paste entry point and to
    // submission, so a \r never reaches the wire either way.
    let mut b = InputBuffer::new();
    b.insert_str("a\rb");
    assert_eq!(b.text(), "a\rb");

    // Insert mid-line lands at the cursor, splitting the existing text.
    let mut b = InputBuffer::new();
    b.set_text("abcd"); // cursor at end (pos 4)
    b.move_left(); // "abc|d"
    b.move_left(); // "ab|cd"
    b.insert_str("XY");
    assert_eq!(b.text(), "abXYcd");

    // A multi-line CJK paste round-trips byte-for-byte.
    let mut b = InputBuffer::new();
    b.insert_str("你好\n世界");
    assert_eq!(b.text(), "你好\n世界");
    assert_eq!(b.line_count(), 2);
}

/// The pre-batch `insert_str` algorithm: one `put_char` per character.
/// Kept as the reference the bulk implementation must match exactly.
fn insert_str_char_by_char(b: &mut InputBuffer, s: &str) {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => b.newline(),
            '\r' if chars.peek() == Some(&'\n') => {}
            _ => b.put_char(c),
        }
    }
}

#[test]
fn insert_str_bulk_matches_char_by_char_reference() {
    // P2-3: `insert_str` splices whole runs instead of `put_char`-ing
    // each byte (String::insert shifts the tail per char — O(n²) on a
    // MB-scale single-line paste). The bulk version must leave the buffer
    // and cursor byte-identical to the per-char reference on every shape
    // of paste: CRLF pairs, lone CRs, CJK/emoji (multi-byte), line breaks
    // at both ends, pastes into a mid-cursor multi-row buffer, and a
    // large generated blob.
    let cases: Vec<&str> = vec![
        "",
        "\n",
        "\r",
        "\r\n",
        "\r\n\r\n",
        "a\rb\r\nc",
        "line1\r\nline2\nline3\r",
        "\nlead",
        "tail\n",
        "你好\r\n世界\r\r🦀x",
        "start\u{1F980}mid\rlong run of ascii text without breaks end",
    ];
    // A large deterministic blob mixing every shape (the O(n²) shape — a
    // huge single-line run — included).
    let mut big = String::new();
    for i in 0..2000 {
        big.push_str(&format!("chunk{i} "));
        if i % 37 == 0 {
            big.push_str("\r\n");
        }
        if i % 53 == 0 {
            big.push('\r');
        }
    }
    let cases = {
        let mut c = cases;
        c.push(&big);
        c
    };
    for s in &cases {
        // Fresh buffers, paste at the end.
        let mut bulk = InputBuffer::new();
        bulk.insert_str(s);
        let mut reference = InputBuffer::new();
        insert_str_char_by_char(&mut reference, s);
        assert_eq!(
            (bulk.text(), bulk.row, bulk.col),
            (reference.text(), reference.row, reference.col),
            "end-paste mismatch for {s:.60?}"
        );
        // Multi-row buffer, cursor mid-line: set_text parks at the end of
        // the last line; one move_up lands on row 1 at col 2 → "ab|cd".
        let mut bulk = InputBuffer::new();
        let mut reference = InputBuffer::new();
        for b in [&mut bulk, &mut reference] {
            b.set_text("xx\nabcd\nyy");
            b.move_up(); // row 1 ("abcd"), col clamped from 2 → "ab|cd"
        }
        bulk.insert_str(s);
        insert_str_char_by_char(&mut reference, s);
        assert_eq!(
            (bulk.text(), bulk.row, bulk.col),
            (reference.text(), reference.row, reference.col),
            "mid-paste mismatch for {s:.60?}"
        );
        // Repeated pastes into the SAME buffer (state carries over).
        let mut bulk = InputBuffer::new();
        let mut reference = InputBuffer::new();
        for chunk in ["first\n", "\r\nsecond\r", "三\n四", s] {
            bulk.insert_str(chunk);
            insert_str_char_by_char(&mut reference, chunk);
        }
        assert_eq!(
            (bulk.text(), bulk.row, bulk.col),
            (reference.text(), reference.row, reference.col),
            "sequence mismatch ending {s:.60?}"
        );
    }
}

#[test]
fn newline_splits_line() {
    let mut b = InputBuffer::new();
    b.set_text("abcd");
    b.move_left();
    b.move_left(); // cursor at col 2
    b.newline();
    assert_eq!(b.text(), "ab\ncd");
    assert_eq!(b.line_count(), 2);
    assert_eq!((b.row, b.col), (1, 0));
}

#[test]
fn backspace_joins_lines() {
    let mut b = InputBuffer::new();
    b.set_text("ab\ncd"); // cursor at end of "cd"
    b.home(); // start of line 1
    b.backspace(); // join → "abcd", cursor after "ab"
    assert_eq!(b.text(), "abcd");
    assert_eq!(b.line_count(), 1);
    assert_eq!((b.row, b.col), (0, 2));
}

#[test]
fn backspace_preserves_unicode() {
    let mut b = InputBuffer::new();
    b.set_text("中文");
    b.move_left(); // before 文
    b.backspace(); // delete 中
    assert_eq!(b.text(), "文");
    assert_eq!(b.col, 0);
}

#[test]
fn delete_forward_deletes_char_or_joins_next_line() {
    // Mid-line: deletes the char AFTER the caret; caret stays put.
    let mut b = InputBuffer::new();
    b.set_text("abcd"); // cursor at end
    b.move_left(); // "abc|d"
    b.move_left(); // "ab|cd"
    b.delete_forward(); // delete 'c'
    assert_eq!(b.text(), "abd");
    assert_eq!(b.col, 2);
    // At the end of a line: joins the NEXT line onto this one (forward
    // mirror of backspace's join-onto-previous).
    let mut b = InputBuffer::new();
    b.set_text("ab\ncd"); // cursor at end of "cd" → move up to "ab"
    b.move_up();
    b.end();
    b.delete_forward();
    assert_eq!(b.text(), "abcd");
    assert_eq!(b.line_count(), 1);
    assert_eq!((b.row, b.col), (0, 2));
    // CJK: deletes the whole wide char, not one of its bytes.
    let mut b = InputBuffer::new();
    b.set_text("中文");
    b.home(); // before 中
    b.delete_forward();
    assert_eq!(b.text(), "文");
    assert_eq!(b.col, 0);
    // No-op at the very end of the buffer.
    let mut b = InputBuffer::new();
    b.set_text("end");
    b.delete_forward();
    assert_eq!(b.text(), "end");
}

#[test]
fn backslash_enter_becomes_a_newline_instead_of_sending() {
    // The `\`+Enter line continuation the Enter handler applies: the same
    // op sequence (end → backspace → newline) pinned here so refactors of
    // the handler keep the semantics. A trailing backslash on the last
    // line turns Enter into a newline — bash semantics, and the one
    // multiline path for terminals that don't report Shift/Alt+Enter.
    let mut b = InputBuffer::new();
    b.set_text("abc\\"); // cursor already at end
    assert!(b.lines_view().last().unwrap().ends_with('\\'));
    b.end();
    b.backspace(); // drop the backslash
    b.newline();
    assert_eq!(b.text(), "abc\n");
    assert_eq!(b.line_count(), 2);
    // No trailing backslash: the same keys would just send — the guard is
    // the ends_with check, not the op sequence.
    let mut b = InputBuffer::new();
    b.set_text("plain");
    assert!(!b.lines_view().last().unwrap().ends_with('\\'));
}

#[test]
fn history_recall_walks_back_and_restores_the_draft() {
    // The shared history_up/history_down both Alt+Up/Down and plain Up/Down
    // route through: first recall saves the in-progress draft, Up walks
    // older, Down walks newer, past the newest restores the draft.
    let history = vec!["one".to_string(), "two".to_string(), "three".to_string()];
    let mut idx = None;
    let mut draft = String::new();
    let mut b = InputBuffer::new();
    b.set_text("draft text");

    history_up(&history, &mut idx, &mut draft, &mut b); // → three (newest)
    assert_eq!(idx, Some(2));
    assert_eq!(b.text(), "three");
    assert_eq!(draft, "draft text");
    history_up(&history, &mut idx, &mut draft, &mut b); // → two
    assert_eq!(idx, Some(1));
    assert_eq!(b.text(), "two");
    history_up(&history, &mut idx, &mut draft, &mut b); // → one
    history_up(&history, &mut idx, &mut draft, &mut b); // clamped at oldest
    assert_eq!(idx, Some(0));
    assert_eq!(b.text(), "one");
    history_down(&history, &mut idx, &draft, &mut b); // → two
    assert_eq!(idx, Some(1));
    history_down(&history, &mut idx, &draft, &mut b); // → three
    history_down(&history, &mut idx, &draft, &mut b); // past newest
    assert_eq!(idx, None);
    assert_eq!(b.text(), "draft text");
    // Empty history: no-op, no panic.
    let empty: Vec<String> = Vec::new();
    history_up(&empty, &mut idx, &mut draft, &mut b);
    assert_eq!(idx, None);
    assert_eq!(b.text(), "draft text");
}

#[test]
fn up_is_history_for_single_line_or_active_browse() {
    // Single-line (incl. empty) buffers recall; multi-line buffers keep
    // caret motion unless a browse is already active.
    assert!(up_is_history(true, false));
    assert!(up_is_history(true, true));
    assert!(!up_is_history(false, false));
    assert!(up_is_history(false, true));
}

#[test]
fn transcript_rows_rebuilds_history_with_tool_pairing() {
    // The resumed-session transcript: text flattens per message, tool
    // blocks interleave in order, results pair with their call by
    // tool_use_id, unknown ids fall back to a placeholder name.
    let msgs = vec![
        Message::user(vec![Block::Text("do it".into())]),
        Message::assistant(vec![
            Block::Text("running".into()),
            Block::ToolUse {
                id: "t1".into(),
                name: "shell".into(),
                input: serde_json::json!({"cmd": "ls"}),
            },
        ]),
        Message::user(vec![Block::ToolResult {
            tool_use_id: "t1".into(),
            content: "a.txt".into(),
            image: None,
            is_error: false,
        }]),
        Message::assistant(vec![Block::Text("done".into())]),
        // A user message mixing text with an orphaned result (id with no
        // matching call) exercises both fallbacks in one message.
        Message::user(vec![
            Block::Text("orphan:".into()),
            Block::ToolResult {
                tool_use_id: "gone".into(),
                content: "?".into(),
                image: None,
                is_error: true,
            },
        ]),
    ];
    let rows = transcript_rows(&msgs);
    let shape = |r: &Row| match r {
        Row::User(_) => "U",
        Row::Assistant(_) => "A",
        Row::ToolCall { .. } => "C",
        Row::ToolResult { .. } => "R",
        _ => "?",
    };
    assert_eq!(
        rows.iter().map(shape).collect::<Vec<_>>(),
        vec!["U", "A", "C", "R", "A", "U", "R"],
        "flattened order: {rows:?}"
    );
    assert!(matches!(&rows[0], Row::User(t) if t == "do it"));
    assert!(matches!(&rows[1], Row::Assistant(t) if t == "running"));
    // The paired result names its tool and carries elapsed=None.
    match &rows[3] {
        Row::ToolResult {
            name,
            is_error,
            elapsed,
            ..
        } => {
            assert_eq!(name, "shell");
            assert!(!is_error);
            assert_eq!(elapsed, &None, "resumed results have no duration");
        }
        other => panic!("expected a ToolResult, got {other:?}"),
    }
    // The orphan: text flushed first, result under the placeholder name.
    assert!(matches!(&rows[5], Row::User(t) if t == "orphan:"));
    match &rows[6] {
        Row::ToolResult { name, is_error, .. } => {
            assert_eq!(name, "(unknown)");
            assert!(*is_error);
        }
        other => panic!("expected a ToolResult, got {other:?}"),
    }
}

#[test]
fn transcript_rows_mark_attachments_and_preserve_text_order() {
    // Attachment bytes stay in the agent's context while lightweight rows
    // keep their position in the resumed conversation understandable.
    let msgs = vec![Message::user(vec![
        Block::Text("look".into()),
        Block::Image {
            media_type: "image/png".into(),
            data: "iVBOR…".into(),
        },
        Block::Document {
            media_type: "application/pdf".into(),
            data: "JVBERi0=".into(),
        },
        Block::Text("at this".into()),
    ])];
    let rows = transcript_rows(&msgs);
    assert_eq!(rows.len(), 4, "text and attachment markers: {rows:?}");
    assert!(matches!(&rows[0], Row::User(t) if t == "look"));
    assert!(matches!(&rows[1], Row::User(t) if t == "[image attached]"));
    assert!(matches!(&rows[2], Row::User(t) if t == "[PDF document attached]"));
    assert!(matches!(&rows[3], Row::User(t) if t == "at this"));
    // Empty history → empty rows (fresh session path).
    assert!(transcript_rows(&[]).is_empty());
}

#[test]
fn move_left_right_over_unicode() {
    let mut b = InputBuffer::new();
    b.set_text("a中b"); // bytes: a(1) 中(3) b(1) = 5
    b.move_left(); // before b → col 4
    b.move_left(); // before 中 → col 1
    assert_eq!(b.col, 1);
    b.move_left(); // before a → col 0
    assert_eq!(b.col, 0);
    b.move_right(); // before 中 → col 1
    assert_eq!(b.col, 1);
}

#[test]
fn move_up_down_clamps_col() {
    let mut b = InputBuffer::new();
    b.set_text("long line one\n短"); // cursor at end of 短 (col 3)
    b.move_up(); // col clamps to min(3, line0.len())
    assert_eq!(b.row, 0);
    assert!(b.col <= "long line one".len());
}

#[test]
fn move_up_down_floors_to_char_boundary() {
    // col is a byte offset valid on the current line. Moving to a line whose
    // multibyte layout differs can place it mid-codepoint — cursor() then
    // slices lines[row][..col] and panics. The mover must floor to a boundary.
    let mut b = InputBuffer::new();
    b.set_text("中\na"); // ["中"(3B), "a"(1B)]; cursor at end of "a": row1 col1
    b.move_up(); // -> row0; col 1 would be inside '中' (bytes 0..3)
    assert_eq!(b.row, 0);
    assert!(
        b.col == 0 || b.col == 3,
        "col {} not a boundary of '中'",
        b.col
    );
    let _ = layout_composer(&b, 20, "❯ ", Style::default()); // slices at the caret; must not panic

    // Reverse direction: cursor after 'a' (col1) on a 1-byte line, moving
    // down onto '中' lands col1 inside it.
    let mut b = InputBuffer::new();
    b.set_text("a\n中"); // ["a", "中"]; cursor ends at row1 col3 (end of 中)
    b.home(); // row1 col0
    b.move_up(); // row0 col0
    b.end(); // row0 col1 (after 'a')
    b.move_down(); // -> row1; col 1 inside '中'
    assert_eq!(b.row, 1);
    assert!(
        b.col == 0 || b.col == 3,
        "col {} not a boundary of '中'",
        b.col
    );
    let _ = layout_composer(&b, 20, "❯ ", Style::default()); // must not panic
}

#[test]
fn home_end() {
    let mut b = InputBuffer::new();
    b.set_text("abc");
    b.home();
    assert_eq!(b.col, 0);
    b.end();
    assert_eq!(b.col, 3);
}

#[test]
fn drain_resets_to_empty() {
    let mut b = InputBuffer::new();
    b.set_text("x\ny\nz");
    let s = b.drain_to_string();
    assert_eq!(s, "x\ny\nz");
    assert!(b.is_empty());
    assert_eq!(b.line_count(), 1);
}

#[test]
fn classify_messages_vs_commands() {
    assert_eq!(classify("hello"), SlashCmd::Message);
    assert_eq!(classify("/clear"), SlashCmd::Clear);
    assert_eq!(classify("/model "), SlashCmd::Model); // trailing space trimmed
    assert_eq!(classify("/limits"), SlashCmd::Limits);
    assert_eq!(classify("/tools"), SlashCmd::Tools);
    assert_eq!(classify("/system"), SlashCmd::System);
    assert_eq!(classify("/context"), SlashCmd::Context);
    assert_eq!(classify("/help"), SlashCmd::Help);
    assert_eq!(classify("/quit"), SlashCmd::Quit);
    assert_eq!(classify("/exit"), SlashCmd::Quit); // alias
                                                   // Case-sensitive: /Model is not /model.
    assert_eq!(classify("/Model"), SlashCmd::Unknown("Model".into()));
    assert_eq!(classify("/foo"), SlashCmd::Unknown("foo".into()));
    // Bare "/" → unknown with empty name (not a message, not a command).
    assert_eq!(classify("/"), SlashCmd::Unknown(String::new()));
    // /export takes an optional path arg, split off + trimmed.
    assert_eq!(classify("/export"), SlashCmd::Export(None));
    assert_eq!(
        classify("/export out.md"),
        SlashCmd::Export(Some("out.md".into()))
    );
    assert_eq!(
        classify("/export   a/b.txt  "),
        SlashCmd::Export(Some("a/b.txt".into()))
    );
    // /export with only trailing spaces → no arg.
    assert_eq!(classify("/export "), SlashCmd::Export(None));
    // No-arg commands reject a trailing arg (only /export and /out take
    // one): `/model foo` is a typo, not a silently-ignored /model.
    assert_eq!(
        classify("/model foo"),
        SlashCmd::Unknown("model foo".into())
    );
    assert_eq!(classify("/clear x"), SlashCmd::Unknown("clear x".into()));
    // /out: optional 1-based index; default (no arg) is the last result.
    assert_eq!(classify("/out"), SlashCmd::Out(None));
    assert_eq!(classify("/out 1"), SlashCmd::Out(Some(1)));
    assert_eq!(classify("/out 3"), SlashCmd::Out(Some(3)));
    // 0 / unparseable / negative all read as unknown (the message names
    // the typo instead of quietly doing something else).
    assert_eq!(classify("/out 0"), SlashCmd::Unknown("out 0".into()));
    assert_eq!(classify("/out x"), SlashCmd::Unknown("out x".into()));
    assert_eq!(classify("/out -1"), SlashCmd::Unknown("out -1".into()));
}

#[test]
fn nth_last_tool_result_clones_expanded_newest_first() {
    let call = Row::ToolCall {
        name: "shell".into(),
        input: serde_json::Value::Null,
        collapsed: String::new(),
        expanded: false,
        started: Instant::now(),
    };
    let mk = |name: &str, is_error: bool| Row::ToolResult {
        name: name.into(),
        content: format!("{name} body"),
        collapsed: format!("{name} body"),
        is_error,
        expanded: false,
        elapsed: None,
    };
    let rows = vec![
        Row::User("q".into()),
        call,
        mk("a", false), // result #3 from the end
        mk("b", true),  // #2 (error results participate)
        Row::sys_info("mid"),
        mk("c", false), // #1 = newest
    ];
    // n=1 (the default) → newest, cloned with expanded set.
    let r = nth_last_tool_result(&rows, 1).expect("newest");
    assert!(matches!(&r, Row::ToolResult { name, expanded: true, .. } if name == "c"));
    // The source row is untouched (still collapsed).
    assert!(
        matches!(
            &rows[5],
            Row::ToolResult {
                expanded: false,
                ..
            }
        ),
        "source row must not be mutated"
    );
    // n=2 skips past the newest.
    assert!(matches!(
        &nth_last_tool_result(&rows, 2).unwrap(),
        Row::ToolResult { name, expanded: true, .. } if name == "b"
    ));
    assert!(matches!(
        &nth_last_tool_result(&rows, 3).unwrap(),
        Row::ToolResult { name, expanded: true, .. } if name == "a"
    ));
    // Past the end / 0 → None.
    assert!(nth_last_tool_result(&rows, 4).is_none());
    assert!(nth_last_tool_result(&rows, 0).is_none());
}

#[test]
fn out_reprint_renders_the_full_guided_body() {
    // The /out clone flows through the normal history rendering: the head
    // matches the original, and the expanded body shows every line under
    // the `│ ` guide.
    let original = Row::ToolResult {
        name: "shell".into(),
        content: "line1\nline2\nline3".into(),
        collapsed: "line1".into(),
        is_error: false,
        expanded: false,
        elapsed: Some("1.2s".into()),
    };
    let reprint = nth_last_tool_result(&[original], 1).expect("the clone");
    let lines = history(&[reprint]);
    let texts: Vec<String> = lines.iter().map(row_text).collect();
    assert!(
        texts.iter().any(|t| t.starts_with("    ▎ ✓ shell · 1.2s")),
        "head: {texts:?}"
    );
    for body in ["    │ line1", "    │ line2", "    │ line3"] {
        assert!(texts.iter().any(|t| t == body), "missing {body}: {texts:?}");
    }
}

#[test]
fn expand_tilde_resolves_home() {
    let home = std::ffi::OsStr::new("/Users/x");
    assert_eq!(
        expand_tilde_with("~/notes.md", Some(home)),
        "/Users/x/notes.md"
    );
    assert_eq!(expand_tilde_with("~", Some(home)), "/Users/x");
    // No leading tilde → passthrough unchanged.
    assert_eq!(expand_tilde_with("foo/bar", Some(home)), "foo/bar");
    assert_eq!(expand_tilde_with("/abs/path", Some(home)), "/abs/path");
    // Only the current user's home; `~other` is left as-is.
    assert_eq!(expand_tilde_with("~other/x", Some(home)), "~other/x");
    // $HOME unset → literal path (no expansion, no error).
    assert_eq!(expand_tilde_with("~/notes.md", None), "~/notes.md");
    assert_eq!(expand_tilde_with("~", None), "~");
}

#[test]
fn render_transcript_dumps_rows_as_plain_text() {
    // /export's escape hatch: the conversation as readable text. Mirrors the
    // on-screen layout (User/Tiny/tool rows), skips mid-stream empty
    // assistant rows, and tags error results distinctly.
    let rows = vec![
        Row::User("hello".into()),
        Row::Thinking {
            text: "pondering".into(),
            tight: false,
        },
        Row::Thinking {
            text: "   ".into(),
            tight: false,
        }, // whitespace-only → skipped
        Row::Assistant(String::new()), // empty → skipped (no bare "Tiny")
        Row::Assistant("I am fine".into()),
        Row::ToolCall {
            name: "shell".into(),
            input: serde_json::Value::Null,
            collapsed: "null".into(),
            expanded: false,
            started: Instant::now(),
        },
        Row::ToolResult {
            name: "shell".into(),
            content: "a.txt".into(),
            collapsed: "a.txt".into(),
            is_error: false,
            expanded: false,
            elapsed: Some("1.2s".into()),
        },
        Row::ToolResult {
            name: "shell".into(),
            content: "boom".into(),
            collapsed: "boom".into(),
            is_error: true,
            expanded: false,
            elapsed: None,
        },
        Row::sys_info("[note]"),
    ];
    let out = render_transcript(&rows);
    assert!(out.contains("You\nhello"), "user row: {out:?}");
    assert!(out.contains("Tiny\nI am fine"), "assistant body: {out:?}");
    assert!(out.contains("✻ Thinking\npondering"), "trace row: {out:?}");
    assert!(
        !out.contains("✻ Thinking\n   "),
        "whitespace-only trace must be skipped"
    );
    assert!(!out.contains("Tiny\n\n"), "empty assistant must be skipped");
    assert!(out.contains("→ shell: null"), "tool call: {out:?}");
    assert!(out.contains("✓ shell: a.txt"), "tool result: {out:?}");
    assert!(
        out.contains("✗ shell: boom"),
        "error result tagged: {out:?}"
    );
    assert!(out.contains("· [note]"), "system row: {out:?}");
}

#[test]
fn fmt_tokens_compact() {
    assert_eq!(fmt_tokens(0), "0");
    assert_eq!(fmt_tokens(999), "999");
    assert_eq!(fmt_tokens(1_000), "1.0k");
    assert_eq!(fmt_tokens(1_500), "1.5k");
    assert_eq!(fmt_tokens(12_000), "12.0k");
    assert_eq!(fmt_tokens(999_999), "1000.0k");
    assert_eq!(fmt_tokens(1_000_000), "1.0M");
    assert_eq!(fmt_tokens(1_500_000), "1.5M");
}

#[test]
fn fmt_tokens_round_drops_only_the_trailing_point_zero() {
    // Below 1k the number prints as-is (nothing to drop).
    assert_eq!(fmt_tokens_round(0), "0");
    assert_eq!(fmt_tokens_round(999), "999");
    // Exact k boundary collapses to the bare integer ("256k" style).
    assert_eq!(fmt_tokens_round(1_000), "1k");
    // 1001 still rounds to 1.0k → "1k"; 1500 keeps its decimal.
    assert_eq!(fmt_tokens_round(1_001), "1k");
    assert_eq!(fmt_tokens_round(1_500), "1.5k");
    // Non-.0 decimals are untouched.
    assert_eq!(fmt_tokens_round(1_100), "1.1k");
    // fmt_tokens rounds 9.999k up to "10.0k", so the .0 drop applies too.
    assert_eq!(fmt_tokens_round(9_999), "10k");
    assert_eq!(fmt_tokens_round(12_000), "12k");
    // 999_999 renders as "1000.0k" (not 1.0M) — the k/M threshold quirk.
    assert_eq!(fmt_tokens_round(999_999), "1000k");
    // The M range mirrors the k range.
    assert_eq!(fmt_tokens_round(1_000_000), "1M");
    assert_eq!(fmt_tokens_round(1_500_000), "1.5M");
    assert_eq!(fmt_tokens_round(2_000_000), "2M");
    assert_eq!(fmt_tokens_round(1_500_000_000), "1500M");
}

#[test]
fn compact_truncates_json_at_two_hundred_chars() {
    // Short values serialize verbatim (serde_json compact form).
    assert_eq!(compact(&serde_json::json!(null)), "null");
    assert_eq!(compact(&serde_json::json!({})), "{}");
    assert_eq!(
        compact(&serde_json::json!({"cmd": "ls"})),
        r#"{"cmd":"ls"}"#
    );
    // The cap is 200 CHARS: exactly 200 serialized chars (198 x's + 2
    // quotes) pass through with no ellipsis.
    let full = serde_json::Value::String("x".repeat(198));
    assert_eq!(compact(&full), format!("\"{}\"", "x".repeat(198)));
    // One char over → hard cut at 200 chars + '…'. The closing quote is
    // dropped mid-string: the preview is display-only, not valid JSON.
    let over = serde_json::Value::String("x".repeat(199));
    let out = compact(&over);
    assert_eq!(out.chars().count(), 201);
    assert!(out.ends_with('…'));
    assert!(!out.ends_with('"'));
    // Chars, not bytes: 150 CJK chars serialize to 450+ bytes but only
    // 152 chars — well under the cap, so nothing is cut.
    let cjk = serde_json::Value::String("看".repeat(150));
    assert_eq!(compact(&cjk), format!("\"{}\"", "看".repeat(150)));
    // 250 CJK chars (252 serialized chars) cut at char 200: the result
    // is 1 quote + 199 wide chars + '…' = 601 bytes, no multibyte tear.
    let wide = serde_json::Value::String("看".repeat(250));
    let out = compact(&wide);
    assert_eq!(out.chars().count(), 201);
    assert!(out.starts_with('"'));
    assert!(out.ends_with('…'));
    assert_eq!(out.len(), 1 + 199 * 3 + 3);
}

/// Helper: a single-span line with one style.
fn span_line(text: &str, fg: Color) -> Line<'static> {
    Line::from(vec![Span::styled(
        text.to_string(),
        Style::default().fg(fg),
    )])
}

/// Display width of a wrapped row (sum of its spans), via unicode-width.
fn row_width(l: &Line) -> usize {
    l.spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

fn test_line_terminal(lines: Vec<Line<'static>>, width: u16, height: u16) -> Terminal<TestBackend> {
    let area = ratatui::layout::Rect::new(0, 0, width, height);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| frame.render_widget(Paragraph::new(lines), area))
        .unwrap();
    terminal
}

fn test_buffer_row(terminal: &Terminal<TestBackend>, y: u16) -> String {
    let buffer = terminal.backend().buffer();
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol().to_string())
        .collect()
}

/// Joined text of a wrapped row (to assert content / dropped spaces).
fn row_text(l: &Line) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[test]
fn wrap_line_short_fits_one_row() {
    let rows = wrap_line(&span_line("hello world", Color::Cyan), 40);
    assert_eq!(rows.len(), 1);
    assert_eq!(row_text(&rows[0]), "hello world");
}

#[test]
fn wrap_line_keeps_the_new_assistant_marks_hanging_indent() {
    let rows = wrap_line(&span_line("◆ alpha beta gamma delta", AGENT_FG), 14);
    assert!(rows.len() > 1);
    assert!(row_text(&rows[0]).starts_with(AGENT_MARK));
    for row in &rows[1..] {
        assert!(
            row_text(row).starts_with("  "),
            "◆ continuation lost its body indent: {:?}",
            row_text(row)
        );
    }
}

#[test]
fn wrap_line_breaks_at_word_boundary_and_drops_space() {
    // width 5: "hello" fits exactly; " world" would overflow → break at the
    // space, drop it, start "world" fresh on row 2. No mid-word split.
    let rows = wrap_line(&span_line("hello world", Color::Cyan), 5);
    assert_eq!(rows.len(), 2);
    assert_eq!(row_text(&rows[0]), "hello");
    assert_eq!(row_text(&rows[1]), "world");
    // The dropped space must not leak as a leading space on the wrap.
    assert!(!row_text(&rows[1]).starts_with(' '));
    // Every row stays within width.
    for r in &rows {
        assert!(row_width(r) <= 5);
    }
}

#[test]
fn wrap_line_hard_breaks_an_overlong_word() {
    // A single word wider than width must still wrap (char-break), not loop
    // forever or overflow the column.
    let rows = wrap_line(&span_line("abcdefgh", Color::Cyan), 3);
    assert!(rows.len() >= 3, "got {} rows", rows.len());
    for r in &rows {
        assert!(row_width(r) <= 3);
    }
    // No characters lost across the hard breaks.
    let joined: String = rows.iter().map(|r| row_text(r)).collect();
    assert_eq!(joined, "abcdefgh");
}

#[test]
fn wrap_line_preserves_span_styles_across_break() {
    // Two styled spans in one line; the wrap splits the second span but its
    // color must survive onto the wrapped row.
    let line = Line::from(vec![
        Span::styled("short ".to_string(), Style::default().fg(Color::Cyan)),
        Span::styled("longtail".to_string(), Style::default().fg(Color::Yellow)),
    ]);
    let rows = wrap_line(&line, 8);
    assert_eq!(rows.len(), 2);
    // Row 0 holds "short " (cyan); row 1 holds "longtail" (yellow) — the
    // split span kept its style.
    assert_eq!(row_text(&rows[0]), "short");
    assert_eq!(row_text(&rows[1]), "longtail");
    assert_eq!(rows[1].spans[0].style.fg, Some(Color::Yellow));
}

#[test]
fn wrap_line_counts_wide_chars_by_display_width() {
    // 6 CJK chars = 12 display columns, no spaces (so one word, hard-broken).
    // At width 4 each row holds 2 chars (4 cols) → 3 rows. Proves width is
    // display columns, not char count — the original bug counted logical
    // lines and under-scrolled CJK text.
    let rows = wrap_line(&span_line("看来离谱的测", Color::Cyan), 4);
    assert_eq!(rows.len(), 3);
    for r in &rows {
        assert!(row_width(r) <= 4);
    }
    // And only at width ≥ 12 do all 6 wide chars fit one row — char-count
    // semantics would have fit them at width 6.
    assert_eq!(
        wrap_line(&span_line("看来离谱的测", Color::Cyan), 11).len(),
        2,
        "11 cols still can't hold 12 cols of wide chars"
    );
    assert_eq!(
        wrap_line(&span_line("看来离谱的测", Color::Cyan), 12).len(),
        1,
        "12 cols exactly fits 6 wide chars on one row"
    );
}

#[test]
fn wrap_line_empty_yields_one_empty_row() {
    let rows = wrap_line(&Line::default(), 10);
    assert_eq!(rows.len(), 1);
    assert_eq!(row_width(&rows[0]), 0);
}

#[test]
fn composer_soft_wraps_cjk_and_emoji_without_losing_the_caret() {
    let mut b = InputBuffer::new();
    b.set_text("甲乙😀Z");
    let composer = layout_composer(&b, 8, "❯ ", Style::default());
    let rows: Vec<String> = composer.lines.iter().map(row_text).collect();
    assert_eq!(rows, ["❯ 甲乙😀", "  Z"]);
    assert!(composer.lines.iter().all(|line| row_width(line) <= 8));
    assert_eq!((composer.caret_row, composer.caret_col), (1, 3));

    b.move_left();
    let composer = layout_composer(&b, 8, "❯ ", Style::default());
    assert_eq!(
        (composer.caret_row, composer.caret_col),
        (1, 2),
        "the caret before Z belongs at the start of the continuation row"
    );
}

#[test]
fn composer_exact_full_width_draws_caret_on_a_real_continuation_cell() {
    use ratatui::backend::Backend;
    use ratatui::layout::Rect;

    let mut b = InputBuffer::new();
    b.set_text("甲乙😀"); // prompt 2 + CJK 4 + emoji 2 = exactly 8 cells
    let composer = layout_composer(&b, 8, "❯ ", Style::default());
    assert_eq!(composer.lines.len(), 2);
    assert_eq!((composer.caret_row, composer.caret_col), (1, 2));

    let area = Rect::new(0, 0, 8, 2);
    let mut terminal = Terminal::new(TestBackend::new(8, 2)).unwrap();
    terminal
        .draw(|frame| {
            frame.render_widget(Paragraph::new(composer.lines), area);
            frame.set_cursor_position((2, 1));
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(2, 0)].symbol(), "甲");
    assert_eq!(buffer[(4, 0)].symbol(), "乙");
    assert_eq!(buffer[(6, 0)].symbol(), "😀");
    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    assert_eq!((cursor.x, cursor.y), (2, 1));
}

#[test]
fn composer_vertical_scroll_keeps_long_multiline_input_and_caret_visible() {
    use ratatui::backend::Backend;
    use ratatui::layout::Rect;

    let mut b = InputBuffer::new();
    b.set_text("one\ntwo\nthree\nfour");
    let composer = layout_composer(&b, 20, "❯ ", Style::default());
    assert_eq!(composer.caret_row, 3);
    let scroll = composer_scroll(composer.caret_row, 2);
    assert_eq!(scroll, 2);

    let area = Rect::new(0, 0, 20, 2);
    let mut terminal = Terminal::new(TestBackend::new(20, 2)).unwrap();
    terminal
        .draw(|frame| {
            frame.render_widget(Paragraph::new(composer.lines).scroll((scroll, 0)), area);
            frame.set_cursor_position((composer.caret_col as u16, 1));
        })
        .unwrap();
    assert_eq!(test_buffer_row(&terminal, 0).trim_end(), "  three");
    assert_eq!(test_buffer_row(&terminal, 1).trim_end(), "  four");
    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    assert_eq!((cursor.x, cursor.y), (6, 1));
}

#[test]
fn composer_narrow_widths_degrade_the_gutter_without_hiding_input() {
    for width in 1..=4 {
        let mut b = InputBuffer::new();
        b.set_text("ab");
        let composer = layout_composer(&b, width, "❯ ", Style::default());
        let visible: String = composer.lines.iter().map(row_text).collect();
        assert!(
            visible.contains('a') && visible.contains('b'),
            "width {width}: {visible:?}"
        );
        assert!(
            composer.lines.iter().all(|line| row_width(line) <= width),
            "width {width}: {:?}",
            composer.lines.iter().map(row_text).collect::<Vec<_>>()
        );
        assert!(
            composer.caret_col < width,
            "width {width}: caret ({}, {})",
            composer.caret_row,
            composer.caret_col
        );
    }
}

#[test]
fn composer_distinguishes_carets_around_a_dropped_wrap_space() {
    let mut before_space = InputBuffer::new();
    before_space.set_text("a b");
    before_space.move_left();
    before_space.move_left();
    let composer = layout_composer(&before_space, 4, "❯ ", Style::default());
    assert_eq!(
        composer.lines.iter().map(row_text).collect::<Vec<_>>(),
        ["❯ a", "  b"]
    );
    assert_eq!((composer.caret_row, composer.caret_col), (0, 3));

    let mut after_space = InputBuffer::new();
    after_space.set_text("a b");
    after_space.move_left();
    let composer = layout_composer(&after_space, 4, "❯ ", Style::default());
    assert_eq!((composer.caret_row, composer.caret_col), (1, 2));
}

#[test]
fn composer_end_caret_stays_on_a_partially_filled_continuation_row() {
    let mut b = InputBuffer::new();
    b.set_text("a bcd");
    let composer = layout_composer(&b, 6, "❯ ", Style::default());
    assert_eq!(
        composer.lines.iter().map(row_text).collect::<Vec<_>>(),
        ["❯ a", "  bcd"]
    );
    assert_eq!((composer.caret_row, composer.caret_col), (1, 5));
}

#[test]
fn grapheme_clusters_never_split_or_distort_the_caret() {
    let rows = wrap_line(&Line::from("👩‍💻"), 2);
    assert_eq!(rows.len(), 1);
    assert_eq!(row_text(&rows[0]), "👩‍💻");

    let mut b = InputBuffer::new();
    b.set_text("❤️");
    let composer = layout_composer(&b, 20, "❯ ", Style::default());
    assert_eq!((composer.caret_row, composer.caret_col), (0, 4));

    // `a` is byte 0; the ZWJ sequence occupies bytes 1..12; `b` starts at 12.
    assert_eq!(display_row_starts("a👩‍💻b", 2), [0, 1, 12]);
}

#[test]
fn top_level_system_marker_keeps_its_hanging_indent() {
    let rows = wrap_line(&Line::from("· aa bb"), 5);
    assert_eq!(
        rows.iter().map(row_text).collect::<Vec<_>>(),
        ["· aa", "  bb"]
    );
}

#[test]
fn md_cache_recomputes_when_text_or_width_changes() {
    // Single-slot cache keyed by (text, width): a hit returns the cached
    // render; ANY other key replaces the slot. Prove reuse vs. recompute
    // by swapping renderers between calls.
    let mut c = MdCache::new();
    let distinguishing = |t: &str| vec![Line::from(t.to_string() + "!")];
    let plain = |t: &str| vec![Line::from(t.to_string())];

    // Miss → the renderer ran.
    let r = c.get_or_render("hi", 40, distinguishing);
    assert_eq!(r[0].spans.first().unwrap().content.as_ref(), "hi!");

    // Hit (same text+width): the plain renderer is IGNORED — still "hi!".
    let r = c.get_or_render("hi", 40, plain);
    assert_eq!(
        r[0].spans.first().unwrap().content.as_ref(),
        "hi!",
        "cache hit must NOT re-render (plain renderer ignored)"
    );

    // Text grew (streaming): miss → the plain renderer now applies.
    let r = c.get_or_render("hi there", 40, plain);
    assert_eq!(r[0].spans.first().unwrap().content.as_ref(), "hi there");

    // Same text at a different terminal width is also a miss — a resize
    // must not reuse the old geometry.
    let r = c.get_or_render("hi there", 24, distinguishing);
    assert_eq!(r[0].spans.first().unwrap().content.as_ref(), "hi there!");

    // The slot is SINGLE: the "hi"@40 entry was evicted by the later
    // keys, so re-requesting it re-renders with the distinguishing
    // renderer instead of returning the stale "hi!".
    let r = c.get_or_render("hi", 40, distinguishing);
    assert_eq!(r[0].spans.first().unwrap().content.as_ref(), "hi!");
    assert_eq!(c.len(), 1);

    // clear() drops the slot → next access is a miss.
    c.clear();
    assert_eq!(c.len(), 0);
    let r = c.get_or_render("hi there", 24, plain);
    assert_eq!(
        r[0].spans.first().unwrap().content.as_ref(),
        "hi there",
        "clear must force a re-render with the new renderer"
    );
}

#[test]
fn md_cache_keys_by_text_not_index() {
    // Regression: the cache was previously keyed by row index, but
    // build_history_lines is called with different slices across frames
    // (commit window vs a single in-flight row via `from_ref`), so the
    // same relative index pointed at different rows. The stale check only
    // compared (text byte-length, width) — two different messages of equal
    // byte length would alias and the wrong cached render would be
    // committed into the terminal scrollback. Keying by (text, width)
    // makes equal-length different messages independent.
    let mut c = MdCache::new();
    let renderer = |t: &str| vec![Line::from(t.to_string())];
    // Two different 4-byte messages ("AAAA" and "BBBB").
    let r1 = c.get_or_render("AAAA", 80, renderer);
    assert_eq!(r1[0].spans.first().unwrap().content.as_ref(), "AAAA");
    // Same width, same byte length, DIFFERENT content → must NOT alias.
    let r2 = c.get_or_render("BBBB", 80, renderer);
    assert_eq!(r2[0].spans.first().unwrap().content.as_ref(), "BBBB");
    // The slot is single, so "BBBB" evicted "AAAA": re-requesting "AAAA"
    // RE-RENDERS (the distinguishing renderer's output comes through)
    // rather than returning either stale entry.
    let distinguishing = |t: &str| vec![Line::from(format!("[{t}]"))];
    let r1_again = c.get_or_render("AAAA", 80, distinguishing);
    assert_eq!(
        r1_again[0].spans.first().unwrap().content.as_ref(),
        "[AAAA]",
        "a single-slot cache re-renders an evicted key instead of aliasing"
    );
}

#[test]
fn md_cache_bounded_single_slot_and_commit_bypass() {
    // P2-1 regression: the cache used to be an unbounded HashMap keyed by
    // (text, width) — during streaming every TextDelta inserted a NEW key
    // (each holding a full Vec<Line> of the message-so-far, so memory
    // accumulated quadratically per message), and committed rows were
    // inserted too despite being rendered exactly once and never read
    // again. Now: occupancy stays ≤1 across a whole simulated stream, and
    // the commit path (LiveRow::None inside push_assistant_body) bypasses
    // the cache so the live entry survives interleaved commits.
    use std::cell::Cell;
    use std::rc::Rc;
    let calls = Rc::new(Cell::new(0usize));
    let counting = {
        let calls = calls.clone();
        move |t: &str| -> Vec<Line<'static>> {
            calls.set(calls.get() + 1);
            vec![Line::from(t.to_string())]
        }
    };
    let mut c = MdCache::new();
    let mut text = String::from("stream head\n");
    for i in 0..100 {
        // A new delta grew the streaming row → live render through the
        // cache (a miss: the key changed).
        text.push_str(&format!("stream delta {i} …\n"));
        let _ = c.get_or_render(&text, 80, &counting);
        assert!(c.len() <= 1, "cache must hold at most one entry");
        // An incremental commit finalized a chunk this frame: the commit
        // render (LiveRow::None) must not touch the slot, which the next
        // live render of the SAME text proves is still a HIT — had the
        // commit gone through the cache, it would have evicted the live
        // entry and forced a re-render (calls+1).
        let committed = vec![Row::Assistant(format!("final paragraph {i}"))];
        let _ = build_history_lines(&committed, &mut c, 80, LiveRow::None, false, true);
        let calls_before = calls.get();
        let _ = c.get_or_render(&text, 80, &counting);
        assert_eq!(
            calls.get(),
            calls_before,
            "commit-path render must bypass the cache (live entry evicted)"
        );
    }
    assert_eq!(c.len(), 1, "exactly the live streaming entry remains");
}

/// Serializes the tests that install the process-global diag sink: install is
/// replace-semantics, so two of them interleaving would swap each other's sink
/// mid-assertion.
static DIAG_SINK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn diag_sink_forwards_kernel_diag_to_ui() {
    let _sink = DIAG_SINK_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // P2-2: while the TUI owns the terminal, kernel diagnostics must
    // arrive as UiMsg::Diag on the UI channel instead of eprintln'ing
    // from a background thread straight over the raw-mode viewport. The
    // macros are pub(crate) in the kernel, so the emission is driven
    // through a public API that diag_warn!s synchronously:
    // session::load on a corrupt file. The sink is process-global, so
    // the test tolerates (and skips past) unrelated diag from parallel
    // tests — same stance as the kernel's own sink test.
    let (tx, rx) = mpsc::channel::<UiMsg>();
    install_diag_bridge(tx);
    let dir = std::env::temp_dir().join(format!("tinyagent-tui-diag-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("corrupt-session.json");
    std::fs::write(&path, "definitely not json").expect("write corrupt session");
    let loaded = tiny_agent::session::load(&path).expect("corrupt session must not error");
    assert!(loaded.is_none(), "a corrupt session starts fresh");

    // The kernel's diag warn must arrive on the UI channel as a Warn
    // payload (send happens synchronously inside load, before recv).
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_diag = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(UiMsg::Diag(tiny_agent::diag::DiagLevel::Warn, text))
                if text.contains("session file corrupt") =>
            {
                saw_diag = true;
                break;
            }
            Ok(_) => continue, // unrelated parallel emission: skip
            Err(_) => break,   // timeout / disconnected: stop scanning
        }
    }
    assert!(
        saw_diag,
        "the kernel diag warn must reach the UI channel as UiMsg::Diag"
    );

    // clear_diag_sink restores the stderr fallback: a second emission
    // after the clear must NOT reach the channel (it goes to eprintln).
    tiny_agent::diag::clear_diag_sink();
    let path2 = dir.join("corrupt-session-2.json");
    std::fs::write(&path2, "also not json").expect("write second corrupt session");
    let _ = tiny_agent::session::load(&path2);
    assert!(
        rx.try_recv().is_err(),
        "no Diag may arrive after clear_diag_sink"
    );
    // Best-effort cleanup (load renames the corrupt files aside).
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn line_to_owned_preserves_spans_style_and_alignment() {
    // M6: the cache stores owned lines; the conversion must keep every
    // span's text + style and the line's alignment, materializing the
    // (possibly borrowed) content into owned Strings.
    use ratatui::layout::Alignment;
    let src = Line::from(vec![
        Span::styled(
            "hello",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" world"),
    ])
    .alignment(Alignment::Center);
    let owned = line_to_owned(&src);
    assert_eq!(owned.spans.len(), 2);
    assert_eq!(owned.spans[0].content.as_ref(), "hello");
    assert_eq!(owned.spans[0].style.fg, Some(Color::Cyan));
    assert!(owned.spans[0].style.add_modifier.contains(Modifier::BOLD));
    assert_eq!(owned.spans[1].content.as_ref(), " world");
    assert_eq!(owned.alignment, Some(Alignment::Center));
    // CJK / multi-byte content round-trips through the owned snapshot too
    // (to_string materializes the whole codepoint sequence, not bytes).
    let cjk = Line::from(vec![Span::raw("你好")]);
    let owned = line_to_owned(&cjk);
    assert_eq!(owned.spans[0].content.as_ref(), "你好");
}

#[test]
fn cached_owned_lines_render_identically_to_fresh_markdown() {
    // M6 end-to-end at the widget layer (the project's TestBackend
    // convention): the cache stores `render_markdown` output run through
    // `line_to_owned`. That snapshot must render into a real buffer with
    // the SAME cells as rendering the borrowed `render_markdown` lines
    // directly — otherwise the cache silently drifts what the user sees.
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    // Header + list + inline code: exercises render_markdown's restyling
    // (cyan bold header, bullet swap, inline-code span).
    let md = "## 标题\n\n- 项目 `代码` more\n";
    let borrowed: Vec<Line> = render_markdown(md, 40);
    let owned: Vec<Line<'static>> = render_markdown(md, 40).iter().map(line_to_owned).collect();

    // Wide enough that the content isn't wrapped (we're comparing caches,
    // not wrapping). Render both into equal buffers and flatten cells.
    let area = Rect::new(0, 0, 40, 5);
    let cells_of = |lines: Vec<Line>| -> String {
        let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        term.draw(|f| f.render_widget(Paragraph::new(lines), area))
            .unwrap();
        let buf = term.backend().buffer();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("│")
    };
    assert_eq!(
        cells_of(borrowed.clone()),
        cells_of(owned.clone()),
        "owned (cached) snapshot must render identically to the borrowed source"
    );
    // Sanity: the buffers carry real content (not both-empty-and-thus-equal).
    // ASCII renders monotonically; CJK wide chars spread across cells (each
    // leaves a trailing empty cell), so check an ASCII fragment plus a CJK
    // glyph rather than the contiguous "标题" substring.
    let rendered = cells_of(borrowed);
    assert!(rendered.contains("more"), "list text missing: {rendered:?}");
    assert!(
        rendered.contains('标'),
        "CJK header glyph missing: {rendered:?}"
    );
}

#[test]
fn render_zone_tail_bottom_aligns_short_content_and_truncates_overflow() {
    // The transcript zone above the input box: content that FITS is padded
    // to the bottom (latest line flush against the input box); content that
    // OVERFLOWS gets a dim `↑ +N` hint on top and the tail below it.
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    let rows_of = |lines: Vec<Line<'static>>, h: u16| -> Vec<String> {
        let area = Rect::new(0, 0, 20, h);
        let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        term.draw(|f| render_zone_tail(f, area, &lines)).unwrap();
        let buf = term.backend().buffer();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    };
    // Fits (1 line in a 3-row zone): bottom-aligned — the top two rows blank.
    let out = rows_of(vec![Line::from("only")], 3);
    assert_eq!(out.len(), 3);
    assert!(
        out[0].trim().is_empty(),
        "top must be blank (bottom-align): {out:?}"
    );
    assert!(out[1].trim().is_empty(), "middle must be blank: {out:?}");
    assert!(
        out[2].trim_start().starts_with("only"),
        "content on the bottom row: {out:?}"
    );
    // Overflow (5 lines in a 3-row zone): the hint takes one row, leaving 2
    // for the tail — so 3 of 5 lines are hidden. Row 0 = hint, rows 1-2 = tail.
    let many: Vec<Line<'static>> = (0..5u32).map(|i| Line::from(format!("line{i}"))).collect();
    let out = rows_of(many, 3);
    assert!(
        out[0].contains('↑'),
        "overflow must show the truncation hint: {out:?}"
    );
    assert!(
        out[0].contains("+3"),
        "hint must name the hidden count (3): {out:?}"
    );
    assert!(
        out[1].trim_start().starts_with("line3"),
        "tail row 1 wrong: {out:?}"
    );
    assert!(
        out[2].trim_start().starts_with("line4"),
        "tail must end on the last line: {out:?}"
    );
}

#[test]
fn slash_popup_lines_windows_to_zone_height_keeping_selection_visible() {
    // 7 commands in a 3-row zone: windowing to the ACTUAL height (not the
    // POPUP_MAX_ROWS ceiling) keeps the highlighted row on-screen wherever
    // the selection is. The old code rendered all 7 then scrolled the tail,
    // stranding the selection off the top.
    let mut p = SlashPopup::new();
    p.update("/"); // every command starts with '/'
    assert_eq!(p.matches.len(), SLASH_COMMANDS.len());
    // Top of the list: window starts at 0, selection (0) is the first row.
    p.sel = 0;
    let top = slash_popup_lines(&p, 3);
    assert_eq!(top.len(), 3, "popup must cap to the zone height");
    assert_eq!(top[0].style.bg, Some(keycap_bg()));
    // Bottom of the list: the window follows so the last selection shows.
    p.sel = SLASH_COMMANDS.len() - 1;
    let bot = slash_popup_lines(&p, 3);
    assert_eq!(bot.len(), 3);
    assert_eq!(
        bot[2].style.bg,
        Some(keycap_bg()),
        "last selection must be the bottom visible row"
    );
    let want = format!(" ❯ /{}", p.matches[p.sel].name);
    assert!(
        bot.iter().any(|l| row_text(l).starts_with(&want)),
        "selected command {want:?} not among visible rows"
    );
}

// ── Word motion / line editing (Alt+←→, Ctrl-W/U/K/A/E) ──────────

#[test]
fn word_boundary_left_finds_previous_word_start() {
    // Caret at end / mid-word → start of "world".
    assert_eq!(word_boundary_left("hello world", 11), 6);
    assert_eq!(word_boundary_left("hello world", 8), 6);
    // A wider gap: caret mid-word still lands on the word start.
    assert_eq!(word_boundary_left("hello  world", 8), 7);
    // Caret on whitespace: the gap AND the word before it are crossed,
    // landing on the previous word's start.
    assert_eq!(word_boundary_left("hello  world", 6), 0);
    // Single word / line start: 0.
    assert_eq!(word_boundary_left("hello", 3), 0);
    assert_eq!(word_boundary_left("hello", 0), 0);
    // CJK has no spaces: a whole non-space run is one word.
    assert_eq!(word_boundary_left("中文 测试", 13), 7);
    assert_eq!(word_boundary_left("中文 测试", 6), 0);
}

#[test]
fn word_boundary_right_finds_next_word_start() {
    // From a word start, mid-word, or on the gap: all land on "world".
    assert_eq!(word_boundary_right("hello world", 0), 6);
    assert_eq!(word_boundary_right("hello world", 2), 6);
    assert_eq!(word_boundary_right("hello world", 5), 6);
    // A wider gap is skipped too.
    assert_eq!(word_boundary_right("hello  world", 5), 7);
    // At the line end it stays put (the buffer crosses to the next line).
    assert_eq!(word_boundary_right("hello", 5), 5);
    // CJK run counts as one word.
    assert_eq!(word_boundary_right("中文 测试", 0), 7);
}

#[test]
fn word_left_right_cross_lines() {
    let mut b = InputBuffer::new();
    b.set_text("foo bar\nbaz qux"); // cursor at end: row 1, col 7
    b.word_left(); // start of "qux"
    assert_eq!((b.row, b.col), (1, 4));
    b.word_left(); // start of "baz"
    assert_eq!((b.row, b.col), (1, 0));
    b.word_left(); // line start → end of the previous line
    assert_eq!((b.row, b.col), (0, 7));
    b.word_left(); // start of "bar"
    assert_eq!((b.row, b.col), (0, 4));

    b.word_right(); // past "bar" → end of line 0
    assert_eq!((b.row, b.col), (0, 7));
    b.word_right(); // line end → start of the next line
    assert_eq!((b.row, b.col), (1, 0));
    b.word_right(); // start of "qux"
    assert_eq!((b.row, b.col), (1, 4));
    b.word_right(); // end of buffer: stays
    assert_eq!((b.row, b.col), (1, 7));
}

#[test]
fn delete_word_back_deletes_word_gap_and_joins_lines() {
    let mut b = InputBuffer::new();
    b.set_text("hello world"); // cursor at end
    b.delete_word_back();
    assert_eq!(b.text(), "hello ");
    assert_eq!(b.col, 6);

    // Caret right after whitespace: the gap plus the word before it go.
    let mut b = InputBuffer::new();
    b.set_text("hello  world"); // cursor at end (col 12)
    for _ in 0..5 {
        b.move_left(); // col 7, right after the two spaces
    }
    b.delete_word_back();
    assert_eq!(b.text(), "world");
    assert_eq!(b.col, 0);

    // A CJK "word" (no internal spaces) deletes as one run.
    let mut b = InputBuffer::new();
    b.set_text("中文 测试"); // cursor at end
    b.delete_word_back();
    assert_eq!(b.text(), "中文 ");
    assert_eq!(b.col, 7);

    // At the start of a line it degrades to backspace (joins lines).
    let mut b = InputBuffer::new();
    b.set_text("ab\ncd");
    b.home(); // row 1, col 0
    b.delete_word_back();
    assert_eq!(b.text(), "abcd");
    assert_eq!((b.row, b.col), (0, 2));
}

#[test]
fn kill_to_line_start_and_end_work_with_unicode() {
    let mut b = InputBuffer::new();
    b.set_text("hello world"); // cursor at end (col 11)
    b.move_left();
    b.move_left();
    b.move_left(); // col 8
    b.kill_to_line_start();
    assert_eq!(b.text(), "rld");
    assert_eq!(b.col, 0);

    let mut b = InputBuffer::new();
    b.set_text("中文 测试"); // cursor at end (col 13)
    b.move_left();
    b.move_left(); // col 7
    b.kill_to_line_start();
    assert_eq!(b.text(), "测试");
    assert_eq!(b.col, 0);

    let mut b = InputBuffer::new();
    b.set_text("hello world");
    b.home();
    b.move_right();
    b.move_right(); // col 2
    b.kill_to_line_end();
    assert_eq!(b.text(), "he");
    assert_eq!(b.col, 2);

    let mut b = InputBuffer::new();
    b.set_text("中文测试"); // cursor at end (col 12)
    b.move_left();
    b.move_left(); // col 6
    b.kill_to_line_end();
    assert_eq!(b.text(), "中文");
    assert_eq!(b.col, 6);
}

// ── worker lifecycle: the diag bridge pins the event channel open ──────

/// The event channel never reports Disconnected while the session lives:
/// `run` installs the diag bridge (holding an `out_tx` clone) before the
/// worker starts, and the bridge is only cleared at TerminalGuard teardown —
/// after `ui_loop` has returned. Even when the worker's own sender dies with
/// its thread (normal exit or panic unwind), `try_recv` keeps returning
/// Empty, which is why a mid-run worker death is detected via the exit flag
/// `DoneSignal` sets, not via channel state.
#[test]
fn diag_bridge_sender_keeps_event_channel_connected_after_worker_exit() {
    let _sink = DIAG_SINK_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // When racing other tests that touch the global sink (install is
    // replace semantics), Disconnected can show up early because the sink
    // was swapped/cleared under us — retry on a fresh channel.
    let mut connected_observed = false;
    for _ in 0..8 {
        let (out_tx, out_rx) = mpsc::channel::<UiMsg>();
        // Mirror run()'s assembly: install the bridge (holding a clone)
        // BEFORE the worker starts.
        install_diag_bridge(out_tx.clone());
        // Simulate the worker closure exiting / unwinding: its sender dies.
        drop(out_tx);
        match out_rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => connected_observed = true,
            Err(mpsc::TryRecvError::Disconnected) => {
                tiny_agent::diag::clear_diag_sink();
                continue; // parallel interference: retry
            }
            // Diag emitted by an unrelated parallel test reached us through
            // the bridge: a live sender, so the channel is still connected.
            Ok(_) => connected_observed = true,
        }
        // Clearing the bridge (the TerminalGuard::drop moment, which in
        // reality happens only after ui_loop returned) is what finally
        // disconnects the channel — the sink clone was the last sender.
        tiny_agent::diag::clear_diag_sink();
        let after_clear = loop {
            match out_rx.try_recv() {
                Ok(_) => continue, // diag queued by a parallel test before the clear
                Err(e) => break e,
            }
        };
        assert!(
            matches!(after_clear, mpsc::TryRecvError::Disconnected),
            "after clear_diag_sink the channel must finally disconnect"
        );
        break;
    }
    tiny_agent::diag::clear_diag_sink(); // process-global sink: always clear
    assert!(
        connected_observed,
        "worker sender dropped: the channel must stay connected (Empty, or a routed diag) while the diag bridge holds its clone"
    );
}

/// reset_after_worker_exit on a mid-run death: the run state fully resets,
/// the death is surfaced as a sys_error row, and the queue dies with the
/// same "[dropped N queued message(s)]" notice the Esc path shows.
#[test]
fn reset_after_worker_exit_resets_run_state_and_drops_queue() {
    let mut phase = RunPhase::Running;
    let mut run_start = Some(Instant::now());
    let mut streaming = true;
    let mut sub_streaming_name = Some("scout".to_string());
    let mut thinking_stream = Some(String::new());
    let mut thinking = true;
    let mut activity = "writing".to_string();
    let mut tool_running = ToolRunning::new();
    tool_running.on_call("shell");
    let mut queued: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    queued.push_back("late question".into());
    let mut rows = vec![Row::User("go".into())];

    let changed = reset_after_worker_exit(
        &mut phase,
        &mut run_start,
        &mut streaming,
        &mut sub_streaming_name,
        &mut thinking_stream,
        &mut thinking,
        &mut activity,
        &mut tool_running,
        &mut queued,
        &mut rows,
    );
    assert!(changed, "a mid-run death changes state");
    assert_eq!(phase, RunPhase::Failed);
    assert!(run_start.is_none());
    assert!(!streaming);
    assert!(sub_streaming_name.is_none());
    assert!(thinking_stream.is_none());
    assert!(!thinking);
    assert!(activity.is_empty());
    assert!(tool_running.calls.is_empty());
    assert!(queued.is_empty());
    let sys: Vec<String> = rows
        .iter()
        .filter_map(|r| match r {
            Row::System { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        sys,
        vec![
            "[worker stopped unexpectedly — type /quit to exit]".to_string(),
            "[dropped 1 queued message(s)]".to_string(),
        ],
        "death surfaced, then the queue notice: {sys:?}"
    );
}

/// reset_after_worker_exit on a death the UI already idled past (or an
/// empty teardown): nothing to reset, nothing queued — no rows, no repaint.
#[test]
fn reset_after_worker_exit_is_a_noop_when_idle_with_no_queue() {
    let mut phase = RunPhase::Idle;
    let mut run_start = None;
    let mut streaming = false;
    let mut sub_streaming_name = None;
    let mut thinking_stream = None;
    let mut thinking = false;
    let mut activity = String::new();
    let mut tool_running = ToolRunning::new();
    let mut queued: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut rows = vec![Row::User("hi".into())];

    let changed = reset_after_worker_exit(
        &mut phase,
        &mut run_start,
        &mut streaming,
        &mut sub_streaming_name,
        &mut thinking_stream,
        &mut thinking,
        &mut activity,
        &mut tool_running,
        &mut queued,
        &mut rows,
    );
    assert!(!changed);
    assert_eq!(rows.len(), 1, "an idle death pushes no rows: {rows:?}");
}

/// ctx_from_usage sums the four provider-reported u64s saturating: a
/// hostile report must neither panic on the debug overflow check (the old
/// plain `+` did) nor wrap into a small number in release.
#[test]
fn ctx_from_usage_saturates_hostile_usage() {
    assert_eq!(ctx_from_usage(u64::MAX - 1, 1, 1, 1), u64::MAX);
    assert_eq!(
        ctx_from_usage(u64::MAX, u64::MAX, u64::MAX, u64::MAX),
        u64::MAX
    );
    // Ordinary reports still sum exactly.
    assert_eq!(ctx_from_usage(100, 20, 30, 10), 160);
}

/// ctx_after_usage is the TUI-03 contract in one function: a direct provider
/// request (`tool_folded == false`) SETs the segment (per-request overwrite,
/// not a sum), while a tool-folded report — a `task` child's multi-turn
/// aggregate that can exceed the whole window — leaves the previous value
/// untouched (its tokens still count into the totals elsewhere).
#[test]
fn ctx_after_usage_sets_on_request_reports_and_holds_on_tool_folds() {
    // Request flavor: overwrite with the report's own numbers (100+20+30+10).
    assert_eq!(ctx_after_usage(999_999, false, 100, 20, 30, 10), 160);
    // Tool-folded flavor: hold whatever the last real request left.
    assert_eq!(ctx_after_usage(54_700, true, 100, 20, 30, 10), 54_700);
    // Holding survives a hostile (window-exceeding) fold: the segment never
    // adopts it — ctx_after_usage ignores the four report values entirely.
    assert_eq!(
        ctx_after_usage(54_700, true, u64::MAX, u64::MAX, u64::MAX, u64::MAX),
        54_700
    );
    // Composition: fold first, then the parent's next real request takes
    // over again with its own measurement.
    let after_fold = ctx_after_usage(160, true, 5_000_000, 0, 0, 900_000);
    assert_eq!(after_fold, 160);
    assert_eq!(
        ctx_after_usage(after_fold, false, 30_000, 2_000, 8_000, 1_500),
        41_500
    );
}

/// A 4-backtick fence cut inside must reopen the tail with the fence's OWN
/// length, not a fixed 3-marker synthetic: the block's own ``` CONTENT line
/// closes a 3-marker opener, and everything after it leaks as markdown.
#[test]
fn soft_cut_4_backtick_fence_tail_reopens_with_its_full_length() {
    // Streaming half-text: an unclosed 4-backtick fence with a ``` line
    // inside; no blank line means no paragraph boundary, so the cut is the
    // display-row soft cut. One display row per line (width 200), 13 rows
    // total — over the threshold.
    let mut text = String::from("````rust\n");
    for i in 0..10 {
        text.push_str(&format!("code-{i:02}\n"));
    }
    text.push_str("```\n"); // inner ``` line: CONTENT inside the 4 fence
    text.push_str("code-tail"); // last line; the closing ```` not written yet

    let mut rows: Vec<Row> = vec![Row::Assistant(text)];
    split_streaming_row(&mut rows, 200);
    assert_eq!(rows.len(), 2, "expected prefix + tail split");
    let (prefix_ok, tail_text, tight) = match (&rows[0], &rows[1]) {
        (Row::Assistant(_), Row::AssistantCont { text: t, tight }) => (true, t.clone(), *tight),
        _ => (false, String::new(), false),
    };
    assert!(prefix_ok, "rows must be Assistant + AssistantCont");
    assert!(tight, "row-level cut is tight");
    // The tail reopens with the opener's true length (4 markers), so the
    // block's own ``` line cannot close the synthetic opener.
    assert!(
        tail_text.starts_with("````\n```"),
        "tail must reopen with a length-true 4-marker fence: {tail_text:?}"
    );
    // Rendering the tail: the inner ``` is code CONTENT and code-tail is
    // code — nothing leaks as markdown.
    let tail_lines = render_markdown(&tail_text, 200);
    let tail_texts: Vec<String> = tail_lines.iter().map(row_text).collect();
    assert!(
        tail_texts.iter().any(|t| t == "▎ ```"),
        "the inner ``` line must render as code content: {tail_texts:?}"
    );
    assert!(
        tail_texts.iter().any(|t| t == "▎ code-tail"),
        "code-tail must render as code: {tail_texts:?}"
    );
    // /export strips the synthetic opener back out (by its length) while
    // keeping the model's own inner ``` line.
    let dumped = render_transcript(&rows);
    assert!(
        !dumped.contains("\n````\n"),
        "synthetic 4-marker opener leaked into the export: {dumped:?}"
    );
    assert!(
        dumped.contains("```\ncode-tail"),
        "the model's own inner ``` line must survive the export: {dumped:?}"
    );
    // The prefix's unclosed fence still renders as one code block, as
    // before — unaffected by the tail-side fix.
    let mut cache = MdCache::new();
    let prefix_lines = build_history_lines(
        std::slice::from_ref(&rows[0]),
        &mut cache,
        200,
        LiveRow::None,
        false,
        false,
    );
    let prefix_texts: Vec<String> = prefix_lines.iter().map(row_text).collect();
    assert_eq!(
        prefix_texts.iter().filter(|t| t.contains("▎ ")).count(),
        10,
        "prefix code lines all barred: {prefix_texts:?}"
    );
}

/// Tool names are model-controlled (execute_tool_call emits whatever the
/// model wrote), so the display name must be sanitized before it reaches row
/// text, the activity segment, or the tool-tracker keys. Control chars are
/// removed; a CSI's parameter bytes stay behind as inert text once the ESC
/// byte is gone (the paint layer filters those too).
#[test]
fn tool_display_name_strips_model_control_chars() {
    let clean = tool_display_name("sh\x1b[31mell\rx\u{0}");
    assert_eq!(clean, "sh[31mellx");
    assert!(!clean.chars().any(char::is_control));
    // \n and \t are legitimate content here, like everywhere strip_controls
    // applies.
    assert_eq!(tool_display_name("a\nb\tc"), "a\nb\tc");
}

// ── /compact [focus] ──────────────────────────────────────────────────────

#[test]
fn slash_compact_grammar_takes_an_optional_focus_hint() {
    assert_eq!(classify("/compact"), SlashCmd::Compact(None));
    assert_eq!(classify("/compact "), SlashCmd::Compact(None)); // trimmed
    assert_eq!(
        classify("/compact focus on the API changes"),
        SlashCmd::Compact(Some("focus on the API changes".into()))
    );
    assert_eq!(
        classify("/compact   keep every endpoint name  "),
        SlashCmd::Compact(Some("keep every endpoint name".into()))
    );
}

#[test]
fn slash_compact_is_listed_idle_only_and_completes_with_a_space() {
    // /help lists it with its arg hint, next to /context.
    let cfg: Config =
        serde_json::from_str(r#"{"provider":"anthropic","api_key_env":"K","model":"m"}"#).unwrap();
    let help = slash_reply(&SlashCmd::Help, &cfg);
    assert!(
        help.lines().any(|l| l.contains("/compact [focus]")),
        "missing from /help: {help}"
    );
    assert!(
        help.contains("Esc cancels it"),
        "the help says the request is cancellable: {help}"
    );
    // Completion: listed when idle, hidden while busy (idle_only), and the
    // completion carries a trailing space because the command takes an arg.
    let mut p = SlashPopup::new();
    p.update_for("/", false);
    let idle: Vec<&str> = p.matches.iter().map(|m| m.name).collect();
    assert!(idle.contains(&"compact"), "idle listing: {idle:?}");
    p.update_for("/", true);
    let busy: Vec<&str> = p.matches.iter().map(|m| m.name).collect();
    assert!(
        !busy.contains(&"compact"),
        "compact is idle-only and must be hidden while busy: {busy:?}"
    );
    p.update("/comp");
    assert_eq!(p.matches.len(), 1);
    assert_eq!(p.matches[0].name, "compact");
    assert_eq!(p.completion().as_deref(), Some("/compact "));
    // Its result comes from the worker, not from slash_reply (the key
    // handler owns the side effect) — the pure reply stays empty.
    assert_eq!(slash_reply(&SlashCmd::Compact(None), &cfg), "");
}

/// `compact_report` is pure: every outcome is pinned without an agent.
#[test]
fn compact_report_covers_every_outcome() {
    let outcome = |dropped: usize, usage: Option<tiny_agent::Usage>| {
        Some(tiny_agent::CompressOutcome {
            dropped,
            folded_ids: Vec::new(),
            kept: 3,
            estimated_tokens_before: 0,
            estimated_tokens_after: 0,
            measured_tokens_before: None,
            usage,
        })
    };
    // A clean fold: no extra row (the Compressed event is it), cost handed over.
    let r = compact_report(Ok(outcome(2, None)), false, 2);
    assert!(r.folded && !r.aborted && r.note.is_none() && r.usage.is_none());
    // The degenerate empty-summary pass: nothing folded, no row of its own
    // (the kernel's Warning event is the row), not an abort.
    let r = compact_report(Ok(outcome(0, None)), false, 2);
    assert!(!r.folded && !r.aborted && r.note.is_none());
    // Nothing to fold: an info row that names the keep-recent setting.
    let r = compact_report(Ok(None), false, 4);
    assert!(!r.folded && !r.aborted && r.usage.is_none());
    let (level, text) = r.note.expect("an explanatory row");
    assert_eq!(level, tiny_agent::diag::DiagLevel::Info);
    assert!(
        text.contains("nothing to fold") && text.contains('4'),
        "{text}"
    );
    // A failed request: a warn row carrying the error text.
    let err = anyhow::anyhow!("summary request rejected: 500");
    let r = compact_report(Err(err), false, 2);
    assert!(!r.folded && !r.aborted && r.usage.is_none());
    let (level, text) = r.note.expect("the failure is reported");
    assert_eq!(level, tiny_agent::diag::DiagLevel::Warn);
    assert!(
        text.contains("[compact] failed") && text.contains("500"),
        "{text}"
    );
}

#[test]
fn compact_report_names_an_aborted_request_and_keeps_its_burned_cost() {
    // The kernel reports a cancelled summary as an Err whose CompressError
    // carries what the aborted request still billed.
    let burned = tiny_agent::provider::mock::mk_usage(1200, 0);
    let inner = tiny_agent::CompressError {
        source: anyhow::anyhow!("summary request did not complete (stop_reason=cancelled)"),
        burned,
    };
    let err = anyhow::Error::new(inner);
    let r = compact_report(Err(err), true, 2);
    assert!(r.aborted && !r.folded, "{r:?}");
    assert!(
        r.note.is_none(),
        "no failure row after a user cancel: {r:?}"
    );
    assert_eq!(r.usage, burned, "the aborted request's bill still lands");
}

#[test]
fn compact_report_prints_the_cause_chain_once() {
    // `CompressError` displays as its `source` AND reports it through
    // `source()`, so `{:#}` on the anyhow wrapper walks both and prints the
    // cause twice ("A: A") — seen on a real screen against a 500 mock.
    let cause = "Mock API error 500 Internal Server Error: {\"error\": \"boom\"}";
    let inner = || tiny_agent::CompressError {
        source: anyhow::Error::new(tiny_agent::provider::ApiStatusError::new(
            500,
            cause.to_string(),
        )),
        burned: Some(tiny_agent::Usage::default()),
    };
    let naive = format!("{:#}", anyhow::Error::new(inner()));
    assert_eq!(
        naive.matches(cause).count(),
        2,
        "the trap this test pins: {naive}"
    );
    let r = compact_report(Err(anyhow::Error::new(inner())), false, 2);
    let (level, text) = r.note.expect("the failure is reported");
    assert_eq!(level, tiny_agent::diag::DiagLevel::Warn);
    assert_eq!(text, format!("[compact] failed: {cause}"));
}

#[test]
fn compact_ledger_bills_the_summary_and_drops_a_stale_ctx_only_after_a_fold() {
    let base = tiny_agent::provider::mock::mk_usage(3, 5).unwrap();
    let usage = tiny_agent::Usage {
        cache_creation_input_tokens: 7,
        cache_read_input_tokens: 11,
        ..base
    };
    let report = |folded: bool, usage: Option<tiny_agent::Usage>| CompactReport {
        folded,
        aborted: false,
        note: None,
        usage,
    };
    // A clean fold: the bill lands in all four totals, the ctx segment goes
    // dark (it described a history that no longer exists).
    let (totals, ctx) = compact_ledger(&report(true, Some(usage)), [100, 200, 300, 400], 54_700);
    assert_eq!(totals, [103, 205, 307, 411]);
    assert_eq!(ctx, 0);
    // Nothing folded: the ctx segment stays — the history it measured is
    // intact — but a request that still billed lands in the totals.
    let (totals, ctx) = compact_ledger(&report(false, Some(usage)), [100, 200, 300, 400], 54_700);
    assert_eq!(totals, [103, 205, 307, 411]);
    assert_eq!(ctx, 54_700);
    // No usage reported (or no request made): the totals are untouched.
    let (totals, ctx) = compact_ledger(&report(false, None), [100, 200, 300, 400], 54_700);
    assert_eq!(totals, [100, 200, 300, 400]);
    assert_eq!(ctx, 54_700);
    // Saturating like every other token accumulator.
    let big = tiny_agent::Usage {
        input_tokens: u64::MAX,
        output_tokens: 10,
        ..Default::default()
    };
    let (totals, _) = compact_ledger(&report(true, Some(big)), [1, u64::MAX, 0, 0], 1);
    assert_eq!(totals, [u64::MAX, u64::MAX, 0, 0]);
}

#[test]
fn compacting_shows_as_a_busy_activity_with_a_cancel_hint() {
    // The composer title while a manual fold is in flight: the activity
    // name, elapsed time and the cancel hint all survive a normal width.
    let title = build_input_title(true, '⠋', "compacting", 3, 0, 80);
    assert!(title.contains("compacting…"), "{title}");
    assert!(title.contains("Ctrl-C cancel"), "{title}");
    // The status bar reads it as a busy run.
    let line = build_status_line("m", "running", true, None, None, "", "", 80);
    assert!(row_text(&line).contains("running"));
}

/// A scratch ToolCtx for a hand-built mock agent (same shape as the harness
/// tests' fixture).
fn compact_test_ctx() -> tiny_agent::ToolCtx {
    tiny_agent::ToolCtx {
        tool_annotations: std::sync::Arc::new(std::collections::BTreeMap::new()),
        hidden_env: Vec::new(),
        tool_absent_notes: Vec::new(),
        workdir: std::env::current_dir().expect("cwd"),
        max_output_chars: 1000,
        restrict_to_workdir: false,
        cancel: None,
        shell_timeout_secs: 120,
        hooks: None,
        subagent: None,
        budget: None,
        deadline: None,
    }
}

/// A mock-backed agent over `history`, keeping its last two messages.
fn compact_test_agent(
    script: Vec<tiny_agent::provider::mock::MockTurn>,
    history: Vec<Message>,
) -> (
    crate::agent::Agent,
    std::sync::Arc<tiny_agent::provider::mock::MockState>,
) {
    let mut cfg: Config = serde_json::from_str(
        r#"{"provider":"openai","api_key_env":"MOCK_API_KEY","model":"m","inject_environment":false}"#,
    )
    .expect("valid test config");
    cfg.limits.max_turns = 5;
    cfg.limits.keep_recent_messages = 2;
    let (prov, state) = tiny_agent::provider::mock::MockProvider::new(script);
    (
        crate::agent::Agent::new(&cfg, Box::new(prov), vec![], history, compact_test_ctx()),
        state,
    )
}

fn compact_history(len: usize) -> Vec<Message> {
    let all = ["task", "a1", "u1", "a2", "u2"];
    all[..len]
        .iter()
        .enumerate()
        .map(|(i, t)| {
            if i % 2 == 0 {
                Message::user(vec![Block::Text((*t).into())])
            } else {
                Message::assistant(vec![Block::Text((*t).into())])
            }
        })
        .collect()
}

/// Everything the worker sent for one `/compact`, in order.
fn drain_compact(
    rx: &mpsc::Receiver<UiMsg>,
) -> (Vec<crate::agent::AgentEvent>, Option<CompactReport>) {
    let mut events = Vec::new();
    let mut report = None;
    while let Ok(msg) = rx.try_recv() {
        match msg {
            UiMsg::Event(ev) => events.push(ev),
            UiMsg::CompactDone(r) => report = Some(r),
            _other => panic!("unexpected message from the worker"),
        }
    }
    (events, report)
}

#[test]
fn compact_step_streams_the_audit_event_then_reports_a_fold() {
    // The worker side end to end on a real agent: the kernel's
    // `Compressed { trigger: Manual }` event goes out first (it is the row
    // the transcript keeps), the single completion report follows it, the
    // history really shrank, and the summary's cost rides the report.
    let (mut agent, state) = compact_test_agent(
        vec![tiny_agent::provider::mock::resp_text(
            "SUMMARY",
            tiny_agent::provider::mock::mk_usage(3, 5),
        )],
        compact_history(5),
    );
    let (tx, rx) = mpsc::channel::<UiMsg>();
    assert!(compact_step(&mut agent, None, &tx));
    let (events, report) = drain_compact(&rx);
    assert_eq!(state.calls(), 1, "one summary request");
    assert_eq!(events.len(), 1, "exactly the audit event: {events:?}");
    assert!(
        matches!(
            &events[0],
            crate::agent::AgentEvent::Compressed {
                trigger: crate::agent::CompressTrigger::Manual,
                dropped: 2,
                kept: 3,
                ..
            }
        ),
        "{events:?}"
    );
    let report = report.expect("one CompactDone");
    assert!(report.folded && !report.aborted && report.note.is_none());
    assert_eq!(
        report.usage,
        tiny_agent::provider::mock::mk_usage(3, 5),
        "the summary's bill is handed to the UI"
    );
    assert_eq!(agent.messages().len(), 3, "the fold really happened");
}

#[test]
fn compact_step_passes_the_focus_hint_to_the_summarizer() {
    let (mut agent, state) = compact_test_agent(
        vec![tiny_agent::provider::mock::resp_text("SUMMARY", None)],
        compact_history(5),
    );
    let (tx, rx) = mpsc::channel::<UiMsg>();
    assert!(compact_step(
        &mut agent,
        Some("focus on the API changes"),
        &tx,
    ));
    let (_, report) = drain_compact(&rx);
    assert!(report.expect("one CompactDone").folded);
    let prompt = state.captured()[0].user_texts.concat();
    assert!(
        prompt.contains("focus on the API changes"),
        "the hint rides the summary prompt: {prompt:?}"
    );
}

#[test]
fn compact_step_reports_nothing_to_fold_and_a_failed_summary() {
    // Too short a history: no request, one info row in the report.
    let (mut agent, state) = compact_test_agent(vec![], compact_history(2));
    let (tx, rx) = mpsc::channel::<UiMsg>();
    assert!(compact_step(&mut agent, None, &tx));
    let (events, report) = drain_compact(&rx);
    assert_eq!(state.calls(), 0, "nothing to summarize: no request");
    assert!(
        events.is_empty(),
        "nothing happened on the agent: {events:?}"
    );
    let report = report.expect("one CompactDone");
    assert!(!report.folded && !report.aborted && report.usage.is_none());
    let (level, text) = report.note.expect("an explanatory row");
    assert_eq!(level, tiny_agent::diag::DiagLevel::Info);
    assert!(text.contains("nothing to fold"), "{text}");
    assert_eq!(agent.messages().len(), 2);

    // A failing summary request: a warn row with the error, no event rows,
    // and the history untouched. Two scripted failures because the kernel
    // retries a transient summary error once before giving up — both
    // attempts fail here, so what the user sees is the real cause.
    let (mut agent, state) = compact_test_agent(
        vec![
            tiny_agent::provider::mock::MockTurn::Err("mock: provider down".into()),
            tiny_agent::provider::mock::MockTurn::Err("mock: provider down".into()),
        ],
        compact_history(5),
    );
    let (tx, rx) = mpsc::channel::<UiMsg>();
    assert!(compact_step(&mut agent, None, &tx));
    let (events, report) = drain_compact(&rx);
    assert_eq!(
        state.calls(),
        2,
        "the initial request plus one transient retry"
    );
    assert!(events.is_empty(), "{events:?}");
    let report = report.expect("one CompactDone");
    assert!(!report.folded && !report.aborted);
    let (level, text) = report.note.expect("the failure is reported");
    assert_eq!(level, tiny_agent::diag::DiagLevel::Warn);
    assert!(text.contains("provider down"), "{text}");
    assert_eq!(
        agent.messages().len(),
        5,
        "a failed fold leaves history alone"
    );
}

#[test]
fn compact_step_reports_a_dead_ui_as_undelivered() {
    // Once the UI is gone (its receiver dropped) the worker must learn it
    // from the return value, like the run path's run_once.
    let (mut agent, _) = compact_test_agent(
        vec![tiny_agent::provider::mock::resp_text("SUMMARY", None)],
        compact_history(5),
    );
    let (tx, rx) = mpsc::channel::<UiMsg>();
    drop(rx);
    assert!(!compact_step(&mut agent, None, &tx));
    // The fold itself still completed on the agent.
    assert_eq!(agent.messages().len(), 3);
}
