use crate::agent::{Agent, AgentEvent};
use crate::config::Config;
use crate::provider::Block;
use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as WBlock, Borders, Paragraph};
use ratatui::Terminal;
use std::io::stdout;
use std::sync::mpsc;
use unicode_width::UnicodeWidthStr;
use std::time::{Duration, Instant};

/// One rendered row in the message history.
enum Row {
    User(String),
    Assistant(String),
    /// Tool call: collapsed shows a compact one-line arg summary; expanded
    /// pretty-prints the full input JSON. `expanded` is toggled in Nav mode.
    /// `collapsed` caches the compact arg summary at creation so the renderer
    /// doesn't re-serialize the input JSON every frame.
    ToolCall {
        name: String,
        input: serde_json::Value,
        collapsed: String,
        expanded: bool,
    },
    /// Tool result: collapsed shows a truncated preview; expanded shows the
    /// full output (which can be large, but participates in normal scrolling).
    ToolResult {
        name: String,
        content: String,
        is_error: bool,
        expanded: bool,
    },
    System(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Normal: typing edits the input box.
    Input,
    /// Browse tool rows: j/k select, Enter/Space toggles expanded, Tab back.
    Nav,
}

/// What a `/`-prefixed input line asks for. `Message` = not a command.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SlashCmd {
    Message,
    Clear,
    Model,
    Limits,
    Tools,
    System,
    Help,
    Unknown(String),
}

/// Classify a trimmed input line into a slash command (or Message). Pure — no
/// side effects — so the command grammar is unit-tested independently of the
/// terminal/key-handling. Case-sensitive: `/Model` is unknown, not `/model`.
fn classify(trimmed: &str) -> SlashCmd {
    let Some(cmd) = trimmed.strip_prefix('/') else {
        return SlashCmd::Message;
    };
    match cmd.trim() {
        "clear" => SlashCmd::Clear,
        "model" => SlashCmd::Model,
        "limits" => SlashCmd::Limits,
        "tools" => SlashCmd::Tools,
        "system" => SlashCmd::System,
        "help" => SlashCmd::Help,
        other => SlashCmd::Unknown(other.to_string()),
    }
}

/// Read-only reply text for an info slash command (everything but Clear, which
/// has its own "[history cleared]" side effect). Pure given the config.
fn slash_reply(cmd: &SlashCmd, cfg: &Config) -> String {
    match cmd {
        SlashCmd::Model => format!(
            "model {} · provider {:?} · max_tokens {} · base {}",
            cfg.model,
            cfg.provider,
            cfg.max_tokens,
            cfg.base_url.as_deref().unwrap_or("(default)"),
        ),
        SlashCmd::Limits => format!(
            "limits: max_turns {} · timeout_secs {}",
            cfg.limits.max_turns, cfg.limits.timeout_secs,
        ),
        SlashCmd::Tools => format!(
            "tools: {}",
            if cfg.tools.is_empty() {
                "(none)".to_string()
            } else {
                cfg.tools.join(", ")
            },
        ),
        SlashCmd::System => format!(
            "system: {}",
            cfg.system_prompt.as_deref().unwrap_or("(none)"),
        ),
        SlashCmd::Help => "commands: /clear /model /limits /tools /system /help".into(),
        SlashCmd::Unknown(name) => format!("[unknown command: /{name} — try /help]"),
        // Message/Clear never reach here; empty keeps the match exhaustive.
        SlashCmd::Message | SlashCmd::Clear => String::new(),
    }
}

/// A row selectable in Nav mode (tool calls and tool results).
fn is_selectable(row: &Row) -> bool {
    matches!(row, Row::ToolCall { .. } | Row::ToolResult { .. })
}

/// Next selectable row index stepping from `from` by `dir` (+1 down / -1 up).
/// Returns None if none exists in that direction; the caller then keeps the
/// current selection, so j/k never silently deselect.
fn next_selectable(rows: &[Row], from: Option<usize>, dir: i32) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }
    let n = rows.len();
    let first = if dir > 0 {
        from.map_or(0, |c| c + 1)
    } else {
        from.map_or(n - 1, |c| c.saturating_sub(1))
    };
    if first >= n {
        return None;
    }
    if dir > 0 {
        (first..n).find(|&i| is_selectable(&rows[i]))
    } else {
        (0..=first).rev().find(|&i| is_selectable(&rows[i]))
    }
}

enum UiMsg {
    Event(AgentEvent),
    Error(String),
    RunFinished,
}

/// Control or text input from the UI to the worker thread. `None` on the
/// channel means "quit" (Esc / idle Ctrl-C); `Some(Text)` is a user message;
/// `Some(Clear)` is the `/clear` command, which the worker applies to the
/// agent's history without running a turn.
enum Input {
    Text(String),
    Clear,
}

/// On drop, signals worker completion so the quit path can wait a bounded
/// time for the worker to exit (and let a cancel-aware tool kill its child)
/// instead of detaching it mid-tool and orphaning a spawned shell process.
struct DoneSignal(mpsc::Sender<()>);

impl Drop for DoneSignal {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// Restores the terminal on drop. Installed right after raw mode + alternate
/// screen are entered, so a panic (or a propagated `?` error) inside the UI
/// loop unwinds through this and still leaves the user's terminal usable —
/// without it, a panic mid-draw strands them in raw mode with no echo.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = stdout().execute(DisableMouseCapture);
        let _ = stdout().execute(DisableBracketedPaste);
        let _ = disable_raw_mode();
        let _ = stdout().execute(LeaveAlternateScreen);
    }
}

pub fn run(
    agent: Agent,
    cfg: &Config,
    initial_task: Option<String>,
    auto_start: bool,
) -> Result<()> {
    // UI → worker: user inputs (None = quit).
    let (in_tx, in_rx) = mpsc::channel::<Option<Input>>();
    // Worker → quit-path: the worker signals it has exited, so quitting can
    // join within a bounded wait instead of detaching mid-tool.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    // Worker → UI: agent events.
    let (out_tx, out_rx) = mpsc::channel::<UiMsg>();
    // Cancellation: Esc sets this; the agent checks it at each turn boundary.
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // The worker thread owns the agent and drives run() per user input.
    let worker_cancel = cancel.clone();
    let worker = std::thread::spawn(move || {
        // Signal completion on ANY exit (normal loop end or early return) via
        // Drop, so the quit path can join within a bounded wait.
        let _done = DoneSignal(done_tx);
        let mut agent = agent;
        agent.set_cancel(worker_cancel);

        // Run one agent turn. Returns false once the UI has gone away
        // (out_rx dropped) so the loop can stop instead of pushing into
        // a dead channel.
        let run_once = |agent: &mut Agent| -> bool {
            let res = agent.run(&mut |ev| {
                let _ = out_tx.send(UiMsg::Event(ev));
            });
            let msg = match res {
                Ok(_) => UiMsg::RunFinished,
                Err(e) => UiMsg::Error(format!("{e:#}")),
            };
            out_tx.send(msg).is_ok()
        };

        // If the session already ends with a user message — an initial
        // task baked in by main, or a resumed session stopped mid-turn at
        // a tool_result — kick off a run now. We must NOT re-send the
        // task over in_tx: main already put it in the agent's history,
        // and sending again would create two adjacent user messages
        // (Anthropic rejects that with a 400).
        if agent.ends_with_user() && !run_once(&mut agent) {
            return;
        }
        while let Ok(Some(cmd)) = in_rx.recv() {
            match cmd {
                Input::Text(text) => {
                    agent.push_user(vec![Block::Text(text)]);
                    if !run_once(&mut agent) {
                        return;
                    }
                }
                Input::Clear => {
                    agent.clear_messages();
                }
            }
        }
    });

    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    // Bracketed paste: terminals that support it deliver a pasted blob as one
    // Event::Paste (newlines included) instead of replaying it as keystrokes —
    // which would hit the Enter handler and send each line as a message.
    stdout().execute(EnableBracketedPaste)?;
    // Mouse capture so the scroll wheel / trackpad two-finger scroll reaches
    // the app (crossterm maps vertical trackpad scroll to ScrollUp/Down). The
    // terminal guard disables it on drop. Note: while captured, click-drag text
    // selection is intercepted by the app — hold Shift to select/copy as usual
    // (most terminals bypass app mouse capture under Shift).
    stdout().execute(EnableMouseCapture)?;
    // From here the terminal is modified; the guard restores it on drop so a
    // panic (or a propagated `?` error) in the UI loop below can't strand the
    // user in raw mode + alternate screen.
    let _guard = TerminalGuard;
    let mut term = Terminal::new(ratatui::backend::CrosstermBackend::new(stdout()))?;
    let result = ui_loop(
        &mut term,
        cfg,
        &in_tx,
        &out_rx,
        &cancel,
        initial_task.as_deref(),
        auto_start,
    );

    // Stop the worker before restoring the terminal. If terminal restoration
    // errors out below, the worker has already been told to stop (and is
    // detached on scope exit) instead of running unchecked to its next limit.
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(in_tx); // signal worker to exit
    // Wait a bounded time for the worker to actually exit. A cancel-aware
    // shell kills its process group within ~50ms, so joining here prevents the
    // orphan-on-quit case where the process exits mid-tool and reparents a
    // running shell to init. A wedged, non-cancel-aware tool (e.g. a stuck MCP
    // call up to its timeout) just lets this time out — we then detach as
    // before, never hanging exit.
    let _ = done_rx.recv_timeout(Duration::from_secs(3));
    drop(worker); // detach if the worker is still stuck past the deadline
    result
    // _guard drops here: restores the terminal (raw mode off, leave alt screen)
    // on the normal path and on any unwind above.
}

fn ui_loop(
    term: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    cfg: &Config,
    in_tx: &mpsc::Sender<Option<Input>>,
    out_rx: &mpsc::Receiver<UiMsg>,
    cancel: &std::sync::atomic::AtomicBool,
    initial_task: Option<&str>,
    auto_start: bool,
) -> Result<()> {
    let mut rows: Vec<Row> = Vec::new();
    let mut input = InputBuffer::new();
    let mut busy = false;
    let mut turn = 0u32;
    let mut scroll_up: usize = 0;
    let mut streaming = false;
    let start = Instant::now();
    // Input history (Alt+Up/Down to recall). `draft` holds the in-progress
    // text when the user starts browsing, so Down past the newest restores it.
    let mut history: Vec<String> = Vec::new();
    let mut hist_idx: Option<usize> = None;
    let mut draft: String = String::new();
    // Running token totals for the session, summed from each turn's usage.
    let mut in_tokens: u64 = 0;
    let mut out_tokens: u64 = 0;
    // Prompt-cache write/read totals — shown in the status bar so the user can
    // see caching paying off (Anthropic reports both; OpenAI reports reads).
    let mut cache_create: u64 = 0;
    let mut cache_read: u64 = 0;
    // Input vs. browse mode (Tab toggles). In Nav mode select_idx picks a
    // tool row; j/k move it, Enter/Space toggles its expanded view.
    let mut mode = Mode::Input;
    let mut select_idx: Option<usize> = None;

    rows.push(Row::System(format!(
        "tiny-agent · model {} · type a message, Enter to send, Esc to quit",
        cfg.model
    )));
    // Mirror the initial task (already in the agent's history) so the
    // user sees what's running. Do NOT re-send it over in_tx: main has
    // already placed it in the agent's messages, and a second send would
    // duplicate the task and create two adjacent user messages (Anthropic
    // rejects that with a 400). The worker starts the run itself.
    if let Some(task) = initial_task {
        rows.push(Row::User(task.to_string()));
    }
    // The worker auto-runs when the session ends with a user message (an
    // initial task, or a resumed session stopped mid-turn). Show "running" so
    // the status bar is honest and the input box blocks instead of queueing a
    // submission behind the in-flight turn.
    if auto_start {
        busy = true;
    }

    loop {
        // Drain worker events.
        loop {
            let msg = match out_rx.try_recv() {
                Ok(m) => m,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Worker thread is gone (e.g. panicked mid-run, or the
                    // process is tearing down). Don't leave the UI stuck in
                    // "running" waiting for a RunFinished that will never come
                    // — surface it so the user can Esc out cleanly.
                    if busy {
                        busy = false;
                        streaming = false;
                        rows.push(Row::System(
                            "[worker stopped unexpectedly — press Esc to quit]".into(),
                        ));
                    }
                    break;
                }
            };
            match msg {
                UiMsg::Event(ev) => match ev {
                    AgentEvent::TurnStart(n) => {
                        turn = n;
                        streaming = false;
                    }
                    AgentEvent::TextDelta(t) => {
                        if !streaming {
                            rows.push(Row::Assistant(String::new()));
                            streaming = true;
                        }
                        if let Some(Row::Assistant(buf)) = rows.last_mut() {
                            buf.push_str(&t);
                        }
                    }
                    AgentEvent::ToolCall { name, input } => {
                        streaming = false;
                        let collapsed = compact(&input);
                        rows.push(Row::ToolCall {
                            name,
                            input,
                            collapsed,
                            expanded: false,
                        });
                    }
                    AgentEvent::ToolResult {
                        name,
                        is_error,
                        content,
                    } => {
                        streaming = false;
                        rows.push(Row::ToolResult {
                            name,
                            content,
                            is_error,
                            expanded: false,
                        });
                    }
                    AgentEvent::Done { stop_reason } => {
                        streaming = false;
                        rows.push(Row::System(format!("[done: {stop_reason}]")));
                    }
                    AgentEvent::Compressed { dropped, kept } => {
                        streaming = false;
                        rows.push(Row::System(format!(
                            "[context compressed: dropped {dropped} messages, {kept} kept]"
                        )));
                    }
                    AgentEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cache_creation_input_tokens,
                        cache_read_input_tokens,
                    } => {
                        in_tokens += input_tokens;
                        out_tokens += output_tokens;
                        cache_create += cache_creation_input_tokens;
                        cache_read += cache_read_input_tokens;
                    }
                },
                UiMsg::Error(e) => {
                    rows.push(Row::System(format!("[error] {e}")));
                    busy = false;
                }
                UiMsg::RunFinished => {
                    busy = false;
                }
            }
        }

        // Render.
        term.draw(|f| {
            let area = f.area();
            // Input box grows with the number of lines typed, up to ~40% of
            // the screen so the conversation always stays visible.
            let max_input = ((area.height as usize) * 2 / 5).max(3);
            let input_h = (input.line_count() + 2).min(max_input) as u16;
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),
                    Constraint::Length(input_h),
                    Constraint::Length(1),
                ])
                .split(area);

            let mut lines: Vec<Line> = Vec::new();
            for (idx, row) in rows.iter().enumerate() {
                let selected = mode == Mode::Nav && select_idx == Some(idx);
                let marker = if selected { "▶ " } else { "  " };
                match row {
                    Row::User(t) => {
                        push_wrapped(&mut lines, "You", t, Color::Cyan);
                    }
                    Row::Assistant(t) => {
                        lines.push(Line::from(Span::styled(
                            "Agent".to_string(),
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                        )));
                        lines.extend(render_markdown(t));
                    }
                    Row::ToolCall {
                        name,
                        input,
                        collapsed,
                        expanded,
                    } => {
                        lines.push(Line::from(Span::styled(
                            format!("{marker}→ {name}"),
                            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        )));
                        let body = if *expanded {
                            serde_json::to_string_pretty(input)
                                .unwrap_or_else(|_| input.to_string())
                        } else {
                            collapsed.clone()
                        };
                        for l in body.lines() {
                            lines.push(Line::from(Span::styled(
                                l.to_string(),
                                Style::default().fg(Color::DarkGray),
                            )));
                        }
                    }
                    Row::ToolResult {
                        name,
                        content,
                        is_error,
                        expanded,
                    } => {
                        let tag = if *is_error { "✗" } else { "✓" };
                        lines.push(Line::from(Span::styled(
                            format!("{marker}{tag} {name}"),
                            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        )));
                        let body = if *expanded {
                            content.clone()
                        } else {
                            crate::agent::preview(content)
                        };
                        let body_color = if *is_error { Color::Red } else { Color::Yellow };
                        for l in body.lines() {
                            lines.push(Line::from(Span::styled(
                                l.to_string(),
                                Style::default().fg(body_color),
                            )));
                        }
                    }
                    Row::System(t) => {
                        push_wrapped(&mut lines, "·", t, Color::DarkGray);
                    }
                }
                lines.push(Line::default());
            }

            let view_height = chunks[0].height.saturating_sub(2) as usize;
            // Pre-wrap to the conversation inner width so the line count is the
            // true rendered row count. Without this, ratatui's `.wrap()` folded
            // long lines at draw time but `lines.len()` counted only logical
            // lines — so the scroll range was too small and the top of a long
            // history (wide code, long tool output) was unreachable. We now
            // wrap ourselves and drop `.wrap()`: our wrapped rows are exactly
            // what's drawn, so the count and the render can't drift apart.
            let inner_w = (chunks[0].width as usize).saturating_sub(2).max(1);
            let wrapped: Vec<Line> = lines
                .iter()
                .flat_map(|l| wrap_line(l, inner_w))
                .collect();
            let total = wrapped.len();
            let max_scroll = total.saturating_sub(view_height);
            if scroll_up > max_scroll {
                scroll_up = max_scroll;
            }
            let scroll = (max_scroll - scroll_up).min(u16::MAX as usize) as u16;

            let history = Paragraph::new(wrapped)
                .block(WBlock::default().borders(Borders::ALL).title("conversation"))
                .scroll((scroll, 0));
            f.render_widget(history, chunks[0]);

            let prompt = if busy {
                "… running (please wait) "
            } else if mode == Mode::Nav {
                "[nav] j/k select · Enter expand · Tab back "
            } else {
                "> "
            };
            let body = if busy || mode == Mode::Nav {
                String::new()
            } else {
                input.text()
            };
            let (crow, ccol) = input.cursor_pos();
            // The input box height is capped (~40% of screen), so a long paste
            // can exceed it. Scroll the box to follow the caret and place the
            // caret within the visible window — otherwise it lands on the
            // status bar or off-screen.
            let inner_rows = (input_h as usize).saturating_sub(2);
            let input_scroll = crow
                .saturating_sub(inner_rows.saturating_sub(1))
                .min(u16::MAX as usize) as u16;
            let input_widget = Paragraph::new(format!("{prompt}{body}"))
                .block(WBlock::default().borders(Borders::ALL).title("input"))
                .scroll((input_scroll, 0));
            f.render_widget(input_widget, chunks[1]);
            // Place the caret. Column = border(1) + display width of the text
            // before the caret (+ the prompt, on the first line only). Display
            // width keeps it correct for wide chars.
            if !busy && mode == Mode::Input {
                let lead = if crow == 0 { prompt.width() } else { 0 };
                let x = chunks[1].x + 1 + (ccol + lead).min(u16::MAX as usize) as u16;
                let y = chunks[1].y + 1 + (crow as u16).saturating_sub(input_scroll);
                f.set_cursor_position((x, y));
            }

            let state = if busy { "running" } else { "idle" };
            let mode_str = match mode {
                Mode::Input => "input",
                Mode::Nav => "nav",
            };
            // Show cache stats only once caching has actually engaged, so the
            // bar stays uncluttered for providers/requests with no caching.
            let cache_seg = if cache_read > 0 || cache_create > 0 {
                format!(
                    " · cache {}w {}r",
                    fmt_tokens(cache_create),
                    fmt_tokens(cache_read)
                )
            } else {
                String::new()
            };
            let status = Line::from(vec![
                Span::styled(
                    format!(" {} ", cfg.model),
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                ),
                Span::raw(format!(
                    "  {state} · {mode_str} · turn {turn} · tok {}↑ {}↓{cache_seg} · {:.0}s",
                    fmt_tokens(in_tokens),
                    fmt_tokens(out_tokens),
                    start.elapsed().as_secs_f64()
                )),
                Span::raw("  Ctrl-C cancel · Tab browse · Esc quit · PgUp/PgDn·wheel scroll"),
            ]);
            f.render_widget(Paragraph::new(status), chunks[2]);
        })?;

        // Keyboard.
        if event::poll(Duration::from_millis(60))? {
            let ev = event::read()?;
            if let Event::Key(key) = ev {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match (key.code, key.modifiers) {
                    (KeyCode::Esc, _) => {
                        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                        let _ = in_tx.send(None);
                        return Ok(());
                    }
                    // Ctrl-C: cancel the in-flight turn (and stay in the TUI);
                    // when idle, fall back to quitting.
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        if busy {
                            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                            // End the current assistant bubble: a System row is
                            // now last, so any trailing TextDelta events still
                            // queued before the stream stops must start a fresh
                            // Assistant row instead of being silently dropped.
                            streaming = false;
                            rows.push(Row::System("[cancelling…]".into()));
                        } else {
                            let _ = in_tx.send(None);
                            return Ok(());
                        }
                    }
                    // Tab toggles between editing input and browsing tool rows.
                    (KeyCode::Tab, _) => {
                        mode = match mode {
                            Mode::Input => {
                                // Snap to the first tool row if nothing is
                                // selected, or if the old selection is stale
                                // (points past the end / at a non-tool row,
                                // e.g. after /clear shrank the list), so j/k
                                // always start from a valid tool row.
                                if select_idx
                                    .is_some_and(|i| i >= rows.len() || !is_selectable(&rows[i]))
                                {
                                    select_idx = None;
                                }
                                if select_idx.is_none() {
                                    select_idx = rows.iter().position(is_selectable);
                                }
                                Mode::Nav
                            }
                            Mode::Nav => Mode::Input,
                        };
                    }
                    // Nav-mode browsing: j/k move the selection among tool rows;
                    // Enter/Space toggles the selected row's expanded view.
                    // (Plain Enter in input mode is handled by the arm below.)
                    (KeyCode::Char('j'), _) if mode == Mode::Nav => {
                        if let Some(n) = next_selectable(&rows, select_idx, 1) {
                            select_idx = Some(n);
                        }
                    }
                    (KeyCode::Char('k'), _) if mode == Mode::Nav => {
                        if let Some(n) = next_selectable(&rows, select_idx, -1) {
                            select_idx = Some(n);
                        }
                    }
                    (KeyCode::Enter, _) | (KeyCode::Char(' '), _) if mode == Mode::Nav => {
                        if let Some(i) = select_idx {
                            if let Some(Row::ToolCall { expanded, .. })
                            | Some(Row::ToolResult { expanded, .. }) = rows.get_mut(i)
                            {
                                *expanded = !*expanded;
                            }
                        }
                    }
                    // Enter sends; Alt/Shift+Enter inserts a newline.
                    (KeyCode::Enter, KeyModifiers::ALT)
                    | (KeyCode::Enter, KeyModifiers::SHIFT) => {
                        if !busy && mode == Mode::Input {
                            input.newline();
                        }
                    }
                    (KeyCode::Enter, _) => {
                        if !busy && !input.is_empty() {
                            let trimmed = input.text().trim().to_string();
                            if !trimmed.is_empty() {
                                match classify(&trimmed) {
                                    SlashCmd::Message => {
                                        if history.last().map(String::as_str) != Some(&trimmed) {
                                            history.push(trimmed.clone());
                                        }
                                        hist_idx = None;
                                        draft.clear();
                                        input.drain_to_string();
                                        rows.push(Row::User(trimmed.clone()));
                                        busy = true;
                                        scroll_up = 0;
                                        let _ = in_tx.send(Some(Input::Text(trimmed)));
                                    }
                                    SlashCmd::Clear => {
                                        // Slash command — handled in the UI,
                                        // never sent to the model. Cleared from
                                        // the input box but NOT recorded in input
                                        // history (so Alt+Up recalls messages,
                                        // not commands).
                                        input.drain_to_string();
                                        scroll_up = 0;
                                        rows.clear();
                                        in_tokens = 0;
                                        out_tokens = 0;
                                        cache_create = 0;
                                        cache_read = 0;
                                        // Reset Nav state: select_idx pointed at
                                        // a now-vanished row, and clearing is an
                                        // input-mode action.
                                        select_idx = None;
                                        mode = Mode::Input;
                                        rows.push(Row::System("[history cleared]".into()));
                                        let _ = in_tx.send(Some(Input::Clear));
                                    }
                                    cmd => {
                                        input.drain_to_string();
                                        scroll_up = 0;
                                        rows.push(Row::System(slash_reply(&cmd, cfg)));
                                    }
                                }
                            }
                        }
                    }
                    (KeyCode::Backspace, _) => {
                        if !busy && mode == Mode::Input {
                            input.backspace();
                        }
                    }
                    (KeyCode::Left, _) => {
                        if !busy && mode == Mode::Input {
                            input.move_left();
                        }
                    }
                    (KeyCode::Right, _) => {
                        if !busy && mode == Mode::Input {
                            input.move_right();
                        }
                    }
                    // Alt+Up/Down recall input history; Up/Down move the caret
                    // across lines inside the buffer.
                    (KeyCode::Up, KeyModifiers::ALT) => {
                        if !busy && mode == Mode::Input && !history.is_empty() {
                            match hist_idx {
                                None => {
                                    draft = input.text();
                                    hist_idx = Some(history.len() - 1);
                                }
                                Some(0) => {}
                                Some(i) => hist_idx = Some(i - 1),
                            }
                            if let Some(i) = hist_idx {
                                input.set_text(&history[i]);
                            }
                        }
                    }
                    (KeyCode::Down, KeyModifiers::ALT) => {
                        if !busy && mode == Mode::Input {
                            if let Some(i) = hist_idx {
                                if i + 1 < history.len() {
                                    hist_idx = Some(i + 1);
                                    input.set_text(&history[i + 1]);
                                } else {
                                    hist_idx = None;
                                    input.set_text(&draft);
                                }
                            }
                        }
                    }
                    (KeyCode::Up, _) => {
                        if !busy && mode == Mode::Input {
                            input.move_up();
                        }
                    }
                    (KeyCode::Down, _) => {
                        if !busy && mode == Mode::Input {
                            input.move_down();
                        }
                    }
                    (KeyCode::Home, _) => {
                        if !busy && mode == Mode::Input {
                            input.home();
                        }
                    }
                    (KeyCode::End, _) => {
                        if !busy && mode == Mode::Input {
                            input.end();
                        }
                    }
                    (KeyCode::PageUp, _) => scroll_up = scroll_up.saturating_add(10),
                    (KeyCode::PageDown, _) => scroll_up = scroll_up.saturating_sub(10),
                    (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                        if !busy && mode == Mode::Input {
                            input.put_char(c);
                        }
                    }
                    _ => {}
                }
            } else if let Event::Paste(s) = ev {
                // Bracketed paste: the whole blob (newlines included) arrives
                // as one event and is inserted as input — not split into many
                // Enter-triggered sends the way a raw paste would be.
                if !busy && mode == Mode::Input {
                    input.insert_str(&s);
                }
            } else if let Event::Mouse(m) = ev {
                // Mouse wheel / trackpad two-finger scroll. Each notch moves a
                // few rows; saturating math keeps scroll_up ≥ 0, and the
                // renderer clamps the top end to max_scroll every frame, so
                // overflow here is harmless. Works while busy too — scrolling
                // back to read earlier output shouldn't wait for the run.
                match m.kind {
                    MouseEventKind::ScrollUp => scroll_up = scroll_up.saturating_add(3),
                    MouseEventKind::ScrollDown => scroll_up = scroll_up.saturating_sub(3),
                    _ => {}
                }
            }
        }
    }
}

/// Render a labeled row: bold colored label line, then content lines.
fn push_wrapped(lines: &mut Vec<Line>, label: &str, text: &str, color: Color) {
    lines.push(Line::from(Span::styled(
        label.to_string(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
    for l in text.lines() {
        lines.push(Line::from(Span::styled(l.to_string(), Style::default().fg(color))));
    }
}

/// Wrap one styled line to `width` display columns, returning one `Line` per
/// visual row. Greedy word wrap: fill a row until the next word won't fit, then
/// break at the last space; a word longer than `width` is hard-broken. A space
/// landing right after a break is dropped (cleaner left edge). Span styles are
/// preserved across breaks — a span split by a wrap becomes two spans with the
/// same style. With this, `lines.len()` equals the rendered height, so the
/// scroll range is exact.
fn wrap_line(src: &Line, width: usize) -> Vec<Line<'static>> {
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

/// Compact token count for the status line: 850, 1.2k, 12k, 1.0M.
fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

fn compact(v: &serde_json::Value) -> String {
    crate::agent::preview(&v.to_string())
}

/// Render a markdown string into styled lines. `tui-markdown` 0.3 styles inline
/// elements (bold / code) correctly but leaves block elements literal: an ATX
/// header renders as plain "## Title" (markers visible, no emphasis) and a list
/// item as "- text". We keep its inline work and fix only the block lines —
/// headers become a bold, level-colored title with the `#` markers stripped, and
/// `-`/`*`/`+` bullets become `•`.
fn render_markdown(md: &str) -> Vec<Line<'_>> {
    let mut lines = tui_markdown::from_str(md).lines;
    for line in lines.iter_mut() {
        let first: &str = line.spans.first().map(|s| s.content.as_ref()).unwrap_or("");
        if let Some(hashes) = header_level(first) {
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
                1 => Color::Yellow,
                2 | 3 => Color::Cyan,
                _ => Color::DarkGray,
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
    }
    lines
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

/// Multi-line input editor: a vec of lines with a (row, col) cursor. `col` is
/// a byte offset kept on UTF-8 char boundaries, so insert/remove never split a
/// multi-byte character. Owned entirely by the UI thread.
struct InputBuffer {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

impl InputBuffer {
    fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    fn put_char(&mut self, c: char) {
        self.lines[self.row].insert(self.col, c);
        self.col += c.len_utf8();
    }

    /// Delete the char before the cursor; at the start of a line, join it onto
    /// the previous one.
    fn backspace(&mut self) {
        if self.col > 0 {
            let prev_ch = self.lines[self.row][..self.col].chars().last().unwrap();
            let prev = self.col - prev_ch.len_utf8();
            self.lines[self.row].replace_range(prev..self.col, "");
            self.col = prev;
        } else if self.row > 0 {
            let cur = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.lines[self.row].push_str(&cur);
        }
    }

    /// Split the current line at the cursor; cursor lands at the start of the
    /// new line.
    fn newline(&mut self) {
        let rest = self.lines[self.row].split_off(self.col);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    /// Insert a (possibly multi-line) string at the cursor — used for bracketed
    /// paste. `\n` splits into a fresh line; a leading `\r` (CRLF pastes) is
    /// dropped so it doesn't leave a stray CR in the input.
    fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            match c {
                '\n' => self.newline(),
                '\r' => {}
                _ => self.put_char(c),
            }
        }
    }

    fn move_left(&mut self) {
        if self.col > 0 {
            let prev_ch = self.lines[self.row][..self.col].chars().last().unwrap();
            self.col -= prev_ch.len_utf8();
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
        }
    }

    fn move_right(&mut self) {
        if self.col < self.lines[self.row].len() {
            let ch = self.lines[self.row][self.col..].chars().next().unwrap();
            self.col += ch.len_utf8();
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = floor_char_boundary(&self.lines[self.row], self.col);
        }
    }

    fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = floor_char_boundary(&self.lines[self.row], self.col);
        }
    }

    fn home(&mut self) {
        self.col = 0;
    }

    fn end(&mut self) {
        self.col = self.lines[self.row].len();
    }

    /// Replace the whole buffer and put the cursor at the end (history recall).
    fn set_text(&mut self, s: &str) {
        self.lines = s.split('\n').map(String::from).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].len();
    }

    /// Take the full text, leaving an empty buffer.
    fn drain_to_string(&mut self) -> String {
        let text = self.lines.join("\n");
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
        text
    }

    fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Snapshot for rendering.
    fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// (row, DISPLAY width of the text before the caret on that line). Display
    /// width — not a char count — so the caret column is correct for wide (CJK)
    /// chars, which occupy two columns each (a char count drifts one column per
    /// wide char and lands the caret mid-text).
    fn cursor_pos(&self) -> (usize, usize) {
        (self.row, self.lines[self.row][..self.col].width())
    }
}

/// Nearest UTF-8 char boundary at or below `idx`. `col` is a byte offset kept
/// on char boundaries of the *current* line; vertical motion targets a line
/// whose multibyte layout may differ, so an offset that's valid here can land
/// mid-codepoint there — and slicing/inserting at a non-boundary
/// (`cursor`/`put_char`/`backspace`/`move_*`) panics. Flooring to a boundary
/// avoids that.
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_headers_and_lists_restyled() {
        // tui-markdown 0.3 leaves ATX headers literal ("## T", unstyled) and
        // bullets literal ("- x"). render_markdown must fix them while keeping
        // inline styling (code → fg White) on list items.
        let lines = render_markdown("## 标题\n\n- 项目 `code` more\n");
        // Header: a single bold span, markers stripped, just the title.
        let hdr = lines
            .iter()
            .find(|l| l.spans.first().is_some_and(|s| s.style.add_modifier.contains(Modifier::BOLD)))
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
    fn caret_column_uses_display_width() {
        // 3 CJK chars = 6 display columns; the caret prefix width must be 6,
        // not the char count 3, or the caret lands mid-text on wide input.
        let mut b = InputBuffer::new();
        for c in "看来离".chars() {
            b.put_char(c);
        }
        assert_eq!(b.cursor_pos(), (0, 6));
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

    #[test]
    fn newline_splits_line() {
        let mut b = InputBuffer::new();
        b.set_text("abcd");
        b.move_left();
        b.move_left(); // cursor at col 2
        b.newline();
        assert_eq!(b.text(), "ab\ncd");
        assert_eq!(b.line_count(), 2);
        assert_eq!(b.cursor_pos(), (1, 0));
    }

    #[test]
    fn backspace_joins_lines() {
        let mut b = InputBuffer::new();
        b.set_text("ab\ncd"); // cursor at end of "cd"
        b.home(); // start of line 1
        b.backspace(); // join → "abcd", cursor after "ab"
        assert_eq!(b.text(), "abcd");
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.cursor_pos(), (0, 2));
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
        assert!(b.col == 0 || b.col == 3, "col {} not a boundary of '中'", b.col);
        let _ = b.cursor_pos(); // slices lines[row][..col]; must not panic

        // Reverse direction: cursor after 'a' (col1) on a 1-byte line, moving
        // down onto '中' lands col1 inside it.
        let mut b = InputBuffer::new();
        b.set_text("a\n中"); // ["a", "中"]; cursor ends at row1 col3 (end of 中)
        b.home(); // row1 col0
        b.move_up(); // row0 col0
        b.end(); // row0 col1 (after 'a')
        b.move_down(); // -> row1; col 1 inside '中'
        assert_eq!(b.row, 1);
        assert!(b.col == 0 || b.col == 3, "col {} not a boundary of '中'", b.col);
        let _ = b.cursor_pos(); // must not panic
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
        assert_eq!(classify("/help"), SlashCmd::Help);
        // Case-sensitive: /Model is not /model.
        assert_eq!(classify("/Model"), SlashCmd::Unknown("Model".into()));
        assert_eq!(classify("/foo"), SlashCmd::Unknown("foo".into()));
        // Bare "/" → unknown with empty name (not a message, not a command).
        assert_eq!(classify("/"), SlashCmd::Unknown(String::new()));
    }

    #[test]
    fn next_selectable_skips_non_tool_rows() {
        let rows = vec![
            Row::User("hi".into()),
            Row::ToolCall {
                name: "a".into(),
                input: serde_json::Value::Null,
                collapsed: String::new(),
                expanded: false,
            },
            Row::Assistant("x".into()),
            Row::ToolResult {
                name: "b".into(),
                content: "c".into(),
                is_error: false,
                expanded: false,
            },
            Row::System("s".into()),
        ];
        // From None: first selectable downward is index 1, upward is 3.
        assert_eq!(next_selectable(&rows, None, 1), Some(1));
        assert_eq!(next_selectable(&rows, None, -1), Some(3));
        // Forward/backward jump over the non-tool Assistant at index 2.
        assert_eq!(next_selectable(&rows, Some(1), 1), Some(3));
        assert_eq!(next_selectable(&rows, Some(3), -1), Some(1));
        // Past the ends → None (caller keeps the current selection).
        assert_eq!(next_selectable(&rows, Some(3), 1), None);
        assert_eq!(next_selectable(&rows, Some(1), -1), None);
    }

    #[test]
    fn next_selectable_empty_or_none_selectable() {
        let empty: Vec<Row> = vec![];
        assert_eq!(next_selectable(&empty, None, 1), None);
        let no_tools = vec![Row::User("a".into()), Row::System("b".into())];
        assert_eq!(next_selectable(&no_tools, None, 1), None);
        assert_eq!(next_selectable(&no_tools, None, -1), None);
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
    fn scroll_reaches_both_ends_of_wrapped_content() {
        // End-to-end at the widget layer: one long line wraps to several rows,
        // and scrolling through [0, max_scroll] must reveal BOTH the first and
        // the last wrapped row. The old code computed max_scroll from the
        // logical line count (1 here), so max_scroll was 0 and everything past
        // the first screenful was unreachable — this pins that the wrap-based
        // count fixes it (and that dropping `.wrap()` still renders the rows).
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;

        let text = "abcdefghijklmnopqrst"; // 20 chars, no spaces → hard-break wrap
        let inner_w = 6usize;
        let wrapped: Vec<Line> = wrap_line(&span_line(text, Color::Cyan), inner_w);
        assert_eq!(wrapped.len(), 4, "20 cols at width 6 → 4 rows");

        let view_h = 2usize;
        let max_scroll = wrapped.len().saturating_sub(view_h);
        assert_eq!(max_scroll, 2);

        let area = Rect::new(0, 0, inner_w as u16 + 2, view_h as u16 + 2);
        let visible = |skip: u16| -> String {
            let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            term.draw(|f| {
                let p = Paragraph::new(wrapped.clone())
                    .block(WBlock::default().borders(Borders::ALL))
                    .scroll((skip, 0));
                f.render_widget(p, area);
            })
            .unwrap();
            let buf = term.backend().buffer();
            (1..=view_h as u16)
                .map(|y| {
                    (1..area.width.saturating_sub(1))
                        .map(|x| buf[(x, y)].symbol().to_string())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("│")
        };

        let top = visible(0);
        let bottom = visible(max_scroll as u16);
        // scroll=0 → first two wrapped rows.
        assert!(top.contains("abcdef") && top.contains("ghijkl"), "top: {top:?}");
        // scroll=max_scroll → last two wrapped rows (previously unreachable).
        assert!(bottom.contains("mnopqr"), "bottom: {bottom:?}");
        assert!(bottom.contains("st"), "bottom tail: {bottom:?}");
    }
}
