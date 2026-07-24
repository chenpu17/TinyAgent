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
    Tool(String),
    System(String),
}

enum UiMsg {
    Event(AgentEvent),
    Error(String),
    RunFinished,
}

pub fn run(
    agent: Agent,
    cfg: &Config,
    initial_task: Option<String>,
    auto_start: bool,
) -> Result<()> {
    // UI → worker: user inputs (None = quit).
    let (in_tx, in_rx) = mpsc::channel::<Option<String>>();
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
        while let Ok(Some(text)) = in_rx.recv() {
            agent.push_user(vec![Block::Text(text)]);
            if !run_once(&mut agent) {
                return;
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
    in_tx: &mpsc::Sender<Option<String>>,
    out_rx: &mpsc::Receiver<UiMsg>,
    cancel: &std::sync::atomic::AtomicBool,
    initial_task: Option<&str>,
    auto_start: bool,
) -> Result<()> {
    let mut rows: Vec<Row> = Vec::new();
    let mut input = String::new();
    let mut busy = false;
    let mut turn = 0u32;
    let mut scroll_up: usize = 0;
    let mut streaming = false;
    let start = Instant::now();

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
                        rows.push(Row::Tool(format!("→ {name} {}", compact(&input))));
                    }
                    AgentEvent::ToolResult {
                        name,
                        is_error,
                        preview,
                    } => {
                        let tag = if is_error { "✗" } else { "✓" };
                        rows.push(Row::Tool(format!("{tag} {name}: {preview}")));
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
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),
                    Constraint::Length(3),
                    Constraint::Length(1),
                ])
                .split(f.area());

            let mut lines: Vec<Line> = Vec::new();
            for row in &rows {
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
                    Row::Tool(t) => {
                        push_wrapped(&mut lines, "Tool", t, Color::Yellow);
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

            let prompt = if busy { "… running (please wait)" } else { "> " };
            let input_widget = Paragraph::new(format!("{prompt}{input}"))
                .block(WBlock::default().borders(Borders::ALL).title("input"));
            f.render_widget(input_widget, chunks[1]);

            let state = if busy { "running" } else { "idle" };
            let status = Line::from(vec![
                Span::styled(
                    format!(" {} ", cfg.model),
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                ),
                Span::raw(format!(
                    "  turn {turn} · {state} · {:.0}s · Esc quit · ↑/↓ scroll",
                    start.elapsed().as_secs_f64()
                )),
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
                    (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                        let _ = in_tx.send(None);
                        return Ok(());
                    }
                    (KeyCode::Enter, _) => {
                        let text = input.trim().to_string();
                        if !text.is_empty() && !busy {
                            input.clear();
                            rows.push(Row::User(text.clone()));
                            busy = true;
                            scroll_up = 0;
                            let _ = in_tx.send(Some(text));
                        }
                    }
                    (KeyCode::Backspace, _) => {
                        input.pop();
                    }
                    (KeyCode::Up, _) => scroll_up = scroll_up.saturating_add(1),
                    (KeyCode::Down, _) => scroll_up = scroll_up.saturating_sub(1),
                    (KeyCode::PageUp, _) => scroll_up = scroll_up.saturating_add(10),
                    (KeyCode::PageDown, _) => scroll_up = scroll_up.saturating_sub(10),
                    (KeyCode::Char(c), _) => input.push(c),
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

fn compact(v: &serde_json::Value) -> String {
    crate::agent::preview(&v.to_string())
}
