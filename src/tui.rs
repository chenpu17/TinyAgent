use crate::agent::{Agent, AgentEvent};
use crate::config::Config;
use crate::provider::Block;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as WBlock, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use std::io::stdout;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// One rendered row in the message history.
enum Row {
    User(String),
    Assistant(String),
    /// Tool call: collapsed shows a compact one-line arg summary; expanded
    /// pretty-prints the full input JSON. `expanded` is toggled in Nav mode.
    ToolCall {
        name: String,
        input: serde_json::Value,
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

pub fn run(
    agent: Agent,
    cfg: &Config,
    initial_task: Option<String>,
    auto_start: bool,
) -> Result<()> {
    // UI → worker: user inputs (None = quit).
    let (in_tx, in_rx) = mpsc::channel::<Option<Input>>();
    // Worker → UI: agent events.
    let (out_tx, out_rx) = mpsc::channel::<UiMsg>();
    // Cancellation: Esc sets this; the agent checks it at each turn boundary.
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // The worker thread owns the agent and drives run() per user input.
    let worker_cancel = cancel.clone();
    let worker = std::thread::spawn(move || {
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
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    drop(worker); // detach: never block exit on a stuck run
    result
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
        while let Ok(msg) = out_rx.try_recv() {
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
                        rows.push(Row::ToolCall {
                            name,
                            input,
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
                    AgentEvent::Usage { input_tokens, output_tokens } => {
                        in_tokens += input_tokens;
                        out_tokens += output_tokens;
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
                        // Bold "Agent" label, then a markdown-rendered body.
                        // from_str borrows `t`; to_owned() clones every Cow into
                        // an owned String so the lines outlive the row borrow.
                        lines.push(Line::from(Span::styled(
                            "Agent".to_string(),
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                        )));
                        let md = tui_markdown::from_str(t).to_owned();
                        lines.extend(md.lines);
                    }
                    Row::ToolCall { name, input, expanded } => {
                        lines.push(Line::from(Span::styled(
                            format!("{marker}→ {name}"),
                            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        )));
                        let body = if *expanded {
                            serde_json::to_string_pretty(input)
                                .unwrap_or_else(|_| input.to_string())
                        } else {
                            compact(input)
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
            let total = lines.len();
            let max_scroll = total.saturating_sub(view_height);
            if scroll_up > max_scroll {
                scroll_up = max_scroll;
            }
            let scroll = (max_scroll - scroll_up) as u16;

            let history = Paragraph::new(lines)
                .block(WBlock::default().borders(Borders::ALL).title("conversation"))
                .wrap(Wrap { trim: false })
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
            let input_widget = Paragraph::new(format!("{prompt}{body}"))
                .block(WBlock::default().borders(Borders::ALL).title("input"));
            f.render_widget(input_widget, chunks[1]);
            // Show the caret when editable. Column offset accounts for the
            // prompt only on the first line; wide chars may drift slightly.
            if !busy && mode == Mode::Input {
                let (crow, ccol) = input.cursor();
                let prompt_chars = prompt.chars().count();
                let lead = if crow == 0 { prompt_chars } else { 0 };
                let x = chunks[1].x + 1 + (ccol + lead) as u16;
                let y = chunks[1].y + 1 + crow as u16;
                f.set_cursor_position((x, y));
            }

            let state = if busy { "running" } else { "idle" };
            let mode_str = match mode {
                Mode::Input => "input",
                Mode::Nav => "nav",
            };
            let status = Line::from(vec![
                Span::styled(
                    format!(" {} ", cfg.model),
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                ),
                Span::raw(format!(
                    "  {state} · {mode_str} · turn {turn} · tok {}↑ {}↓ · {:.0}s",
                    fmt_tokens(in_tokens),
                    fmt_tokens(out_tokens),
                    start.elapsed().as_secs_f64()
                )),
                Span::raw("  Ctrl-C cancel · Tab browse · Esc quit · PgUp/PgDn scroll"),
            ]);
            f.render_widget(Paragraph::new(status), chunks[2]);
        })?;

        // Keyboard.
        if event::poll(Duration::from_millis(60))? {
            if let Event::Key(key) = event::read()? {
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
                                // selected, so j/k have a starting point.
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
            self.col = self.col.min(self.lines[self.row].len());
        }
    }

    fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].len());
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

    /// Cursor (row, char-column) for display. Char column so the caret lands on
    /// a grapheme, not mid-codepoint. (Display width of wide chars is ignored —
    /// the caret may drift on CJK, accepted for now.)
    fn cursor(&self) -> (usize, usize) {
        let col_chars = self.lines[self.row][..self.col].chars().count();
        (self.row, col_chars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn newline_splits_line() {
        let mut b = InputBuffer::new();
        b.set_text("abcd");
        b.move_left();
        b.move_left(); // cursor at col 2
        b.newline();
        assert_eq!(b.text(), "ab\ncd");
        assert_eq!(b.line_count(), 2);
        assert_eq!(b.cursor(), (1, 0));
    }

    #[test]
    fn backspace_joins_lines() {
        let mut b = InputBuffer::new();
        b.set_text("ab\ncd"); // cursor at end of "cd"
        b.home(); // start of line 1
        b.backspace(); // join → "abcd", cursor after "ab"
        assert_eq!(b.text(), "abcd");
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.cursor(), (0, 2));
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
}
