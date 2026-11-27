//! Painting: finished rows into the terminal's scrollback, and the live
//! viewport (transcript zone, queue preview, composer, status bar). Both take
//! the session by `&mut` because the markdown cache is shared between the two;
//! neither changes any other state.

use super::render::{
    build_input_title, build_status_line, chrome_fg, commit_rows, composer_footer, composer_scroll,
    content_width, empty_state_lines, failed_title, layout_composer, queue_preview_line,
    render_zone_tail, render_zone_top, status_stats, truncate_display, wrap_history,
};
use super::session::{RunPhase, Session};
use super::slash::slash_popup_lines_for;
use super::theme::{
    composer_bg, dim_fg, fmt_ctx, surface_bg, AGENT_FG, TOOL_CALL_FG, TOOL_ERR_FG, USER_FG,
};
use super::transcript::{
    build_history_lines, commit_pending_end, inflight_row_index, tool_running_line, LiveRow, Row,
    AGENT_MARK, TOOL_HEAD_MARK,
};
use crate::config::Config;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as WBlock, BorderType, Borders, Paragraph};
use ratatui::{Frame, Terminal};
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

/// Braille spinner frames for the activity shown in the input border title.
/// The frame is derived from the run's elapsed time (100ms per frame), not
/// stored state.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Height of the inline viewport (the live band at the bottom of the primary
/// screen). Splits into a transcript/streaming zone (top), the input box, and
/// a one-row status bar. Tall enough that the slash popup and a few streaming
/// lines fit; the host terminal scrolls once committed content outgrows it.
pub(super) const INLINE_HEIGHT: u16 = 8;

/// Ghost placeholder shown in the empty input box. Keeps the /help pointer:
/// the welcome line scrolls away, this is the standing discoverability hint.
/// Leading space keeps the terminal's block caret off the first glyph of the
/// placeholder — the empty composer reads as focused, not as a selected "D".
const INPUT_PLACEHOLDER: &str = " Describe a task…  ·  / for commands";

/// Commit finalized rows into the terminal scrollback. A row is "finalized"
/// once it will never mutate again: everything except the in-flight streaming
/// Assistant/Thinking row (skipped while its stream is open). Pending input is
/// outside `rows` and never blocks a commit. This is the one place the inline
/// model writes the conversation to scrollback, so the host terminal's native
/// selection/scroll/search cover it.
pub(super) fn commit<B: ratatui::backend::Backend>(
    s: &mut Session,
    term: &mut Terminal<B>,
) -> Result<(), B::Error> {
    let stream_open = s.run.stream_open();
    let pending_end = commit_pending_end(&s.rows, s.committed, stream_open);
    if s.committed < pending_end {
        // Same 120-col readability cap as the live viewport
        // (content_width): committing at the FULL terminal width
        // re-wrapped every finalized row the moment it left the live
        // zone — a visible reflow jump on >120-col terminals, where the
        // live view had rendered at 120 and the scrollback commit
        // suddenly went wider (external re-review).
        let width = content_width(term.size().map(|sz| sz.width).unwrap_or(80));
        commit_rows(
            term,
            &s.rows[s.committed..pending_end],
            &mut s.md_cache,
            width,
            pending_end == s.rows.len(),
        )?;
        s.committed = pending_end;
        s.dirty = true;
    }
    Ok(())
}

/// The frame the terminal keeps. The inline viewport is released, not
/// erased: whatever was last painted stays on screen above the shell prompt.
/// Called once as the loop ends, with the completion popup closed — `/quit`
/// typed in full runs on the first Enter, straight out of the open popup
/// (see `Session::on_popup_key`), and its rows would stay painted otherwise.
/// Best-effort: a terminal that is already gone is the restore guard's error
/// to report, not this one's.
pub(super) fn final_frame<B: ratatui::backend::Backend>(
    s: &mut Session,
    term: &mut Terminal<B>,
    cfg: &Config,
) {
    let text = s.composer.input.text();
    s.composer.popup.dismiss(&text);
    let _ = term.draw(|f| draw(f, s, cfg));
}

/// Paint the live viewport — the INLINE_HEIGHT-row band at the bottom of the
/// primary screen.
pub(super) fn draw(f: &mut Frame, s: &mut Session, cfg: &Config) {
    // Index of the in-flight row shown live in the viewport's transcript
    // zone (None when nothing is streaming). Computed independently of the
    // commit watermark above — see inflight_row_index — so a later streaming
    // Assistant always renders even after earlier rows have finalized.
    let inflight_idx = inflight_row_index(&s.rows, s.run.stream_open());
    let area = f.area(); // the INLINE_HEIGHT-row band at the bottom
    let busy = s.run.phase.busy();
    let term_w = area.width;
    let (prompt, prompt_style) = (
        "❯ ".to_string(),
        Style::default().fg(USER_FG).add_modifier(Modifier::BOLD),
    );
    let chrome_w = content_width(term_w) as u16;
    let inner_cols = (chrome_w as usize).saturating_sub(2).max(1);
    let composer = layout_composer(&s.composer.input, inner_cols, &prompt, prompt_style);
    // Soft-wrapped rows, not logical newlines, determine height.
    // The fixed inline viewport keeps at least a small live zone.
    let max_input = ((area.height as usize) / 2).max(3);
    let input_h = (composer.lines.len() + 2).clamp(3, max_input) as u16;
    let queue_line = queue_preview_line(&s.queued, s.queue_paused, chrome_w as usize);
    let queue_h = if queue_line.is_some() { 1 } else { 0 };
    let [zone_rect, mut queue_rect, mut input_rect, mut status_rect] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(queue_h),
        Constraint::Length(input_h),
        Constraint::Length(1),
    ])
    .areas(area);
    // A 200-column prompt is harder to scan, not more useful. Keep
    // the composer/status on the same readable column as prose.
    queue_rect.width = chrome_w;
    input_rect.width = chrome_w;
    status_rect.width = chrome_w;
    let content_w = content_width(term_w);

    // Transcript zone (the rows above the input box inside the
    // viewport): the slash popup while typing a command, else the
    // thinking placeholder before any output, else the in-flight
    // streaming row, else the running-tool placeholder (a tool
    // execution streams nothing of its own). Each is bottom-aligned
    // so its last line sits flush above the input box.
    // Already-committed turns live in the host terminal's
    // scrollback above this viewport.
    if s.composer.popup.is_open() {
        let lines = slash_popup_lines_for(&s.composer.popup, zone_rect.height as usize, content_w);
        render_zone_top(f, zone_rect, &lines);
    } else if s.run.phase == RunPhase::Idle && s.rows.len() <= 1 && s.composer.input.is_empty() {
        let lines = empty_state_lines(content_w);
        render_zone_top(f, zone_rect, &lines);
    } else if s.run.thinking {
        let line = Line::from(vec![
            Span::styled(
                AGENT_MARK.to_string(),
                Style::default().fg(AGENT_FG).add_modifier(Modifier::BOLD),
            ),
            Span::styled("…", Style::default().fg(dim_fg())),
        ]);
        render_zone_tail(f, zone_rect, &[line]);
    } else if let Some(idx) = inflight_idx {
        if let Some(row) = s.rows.get(idx) {
            let live = if matches!(row, Row::Thinking { .. }) {
                LiveRow::Think
            } else {
                LiveRow::Text
            };
            let l = build_history_lines(
                std::slice::from_ref(row),
                &mut s.md_cache,
                content_w,
                live,
                false,
                false,
            );
            let wrapped = wrap_history(&l, content_w);
            render_zone_tail(f, zone_rect, &wrapped);
        }
    } else if let Some((name, started)) = s.run.tool_building.as_ref().filter(|_| busy) {
        let line = Line::from(vec![
            Span::styled(
                format!("{TOOL_HEAD_MARK}→ {name}"),
                Style::default()
                    .fg(TOOL_CALL_FG)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · receiving arguments · {}s", started.elapsed().as_secs()),
                Style::default().fg(dim_fg()),
            ),
        ]);
        render_zone_tail(f, zone_rect, &[line]);
    } else if let Some(line) = tool_running_line(&s.run.tool_running, Instant::now()) {
        render_zone_tail(f, zone_rect, &[line]);
    }
    if let Some(line) = queue_line {
        f.render_widget(Paragraph::new(line), queue_rect);
    }

    // Input box. The editing prompt is stable; Enter queues while
    // busy. The border frame tracks the run: busy flips the whole
    // frame to AGENT_FG so the bottom chrome reads as one busy
    // unit, idle falls back to USER_FG.
    let spinner_char = SPINNER[(s
        .run
        .run_start
        .map(|t| t.elapsed().as_millis() / 100)
        .unwrap_or(0) as usize)
        % SPINNER.len()];
    let body = s.composer.input.text();
    // Vertical scrolling follows the caret through soft-wrapped
    // rows. No horizontal scroll: long prompts remain reviewable.
    let inner_rows = (input_h as usize).saturating_sub(2);
    let input_scroll = composer_scroll(composer.caret_row, inner_rows);
    let secs = s.run.run_start.map(|t| t.elapsed().as_secs()).unwrap_or(0);
    let act = if s.run.activity.is_empty() {
        "working"
    } else {
        s.run.activity.as_str()
    };
    let title_w = (input_rect.width as usize).saturating_sub(2);
    let input_title = match s.run.phase {
        RunPhase::Cancelling => truncate_display(" ⠹ cancelling… · waiting for stop ", title_w),
        RunPhase::Failed => failed_title(s.queued.len(), title_w),
        _ => build_input_title(busy, spinner_char, act, secs, s.queued.len(), title_w),
    };
    let title_fg = match s.run.phase {
        RunPhase::Cancelling => TOOL_CALL_FG,
        RunPhase::Failed => TOOL_ERR_FG,
        _ => chrome_fg(busy),
    };
    let mut input_lines = composer.lines;
    if body.is_empty() {
        if let Some(first) = input_lines.first_mut() {
            first.spans.push(Span::styled(
                truncate_display(INPUT_PLACEHOLDER, inner_cols.saturating_sub(prompt.width())),
                Style::default().fg(dim_fg()),
            ));
        }
    }
    let border_fg = match s.run.phase {
        RunPhase::Idle => dim_fg(),
        _ => title_fg,
    };
    let mut input_block = WBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().bg(composer_bg()))
        .border_style(Style::default().fg(border_fg))
        .title(Span::styled(input_title, Style::default().fg(title_fg)));
    if let Some(footer) = composer_footer(busy, title_w) {
        input_block = input_block.title_bottom(footer);
    }
    let input_widget = Paragraph::new(input_lines)
        .block(input_block)
        .scroll((input_scroll, 0));
    f.render_widget(input_widget, input_rect);
    let x = input_rect.x + 1 + (composer.caret_col.min(inner_cols.saturating_sub(1)) as u16);
    let y = input_rect.y + 1 + (composer.caret_row as u16).saturating_sub(input_scroll);
    f.set_cursor_position((x, y));

    // Status bar. Detailed run activity lives in the input title;
    // the bar only names the broad state so the two regions do not
    // compete. Idle shows no clock — the run elapsed is the title's
    // job.
    let state = s.run.phase.label().to_string();
    // A brand-new session has nothing to report: showing
    // "0 in / 0 out" is pure noise. The segment appears with the
    // first billed turn (cache-only counters count as activity).
    let stats = status_stats(
        s.usage.in_tokens,
        s.usage.out_tokens,
        s.usage.cache_create,
        s.usage.cache_read,
    );
    let ctx = fmt_ctx(s.usage.ctx_tokens, cfg.limits.max_context_tokens);
    // Plan segment: " · plan done/total", green when every step is
    // completed, yellow while work remains. None when the agent
    // hasn't used `todo` (segment absent, not a null).
    let plan_seg = if s.plan.is_empty() {
        None
    } else {
        let total = s.plan.len();
        let done = s.plan.iter().filter(|t| t.status == "completed").count();
        let style = if done == total {
            Style::default().fg(AGENT_FG)
        } else {
            Style::default().fg(TOOL_CALL_FG)
        };
        Some((format!(" · plan {done}/{total}"), style))
    };
    let status = build_status_line(
        &cfg.model,
        &state,
        busy,
        ctx.as_ref().map(|(t, s)| (t.as_str(), *s)),
        plan_seg.as_ref().map(|(t, s)| (t.as_str(), *s)),
        &stats,
        &s.cwd_path,
        status_rect.width as usize,
    );
    f.render_widget(
        Paragraph::new(status).style(Style::default().bg(surface_bg())),
        status_rect,
    );
}
