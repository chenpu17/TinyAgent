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
            let body = if busy { String::new() } else { input.text() };
            let input_widget = Paragraph::new(format!("{prompt}{body}"))
                .block(WBlock::default().borders(Borders::ALL).title("input"));
            f.render_widget(input_widget, chunks[1]);
            // Show the caret when editable. Column offset accounts for the
            // prompt only on the first line; wide chars may drift slightly.
            if !busy {
                let (crow, ccol) = input.cursor();
                let prompt_chars = prompt.chars().count();
                let lead = if crow == 0 { prompt_chars } else { 0 };
                let x = chunks[1].x + 1 + (ccol + lead) as u16;
                let y = chunks[1].y + 1 + crow as u16;
                f.set_cursor_position((x, y));
            }

            let state = if busy { "running" } else { "idle" };
            let status = Line::from(vec![
                Span::styled(
                    format!(" {} ", cfg.model),
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                ),
                Span::raw(format!(
                    "  turn {turn} · {state} · {:.0}s · Ctrl-C cancel · Esc quit · PgUp/PgDn scroll",
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
                    // Enter sends; Alt/Shift+Enter inserts a newline.
                    (KeyCode::Enter, KeyModifiers::ALT)
                    | (KeyCode::Enter, KeyModifiers::SHIFT) => {
                        if !busy {
                            input.newline();
                        }
                    }
                    (KeyCode::Enter, _) => {
                        if !busy && !input.is_empty() {
                            let trimmed = input.text().trim().to_string();
                            if !trimmed.is_empty() {
                                if history.last().map(String::as_str) != Some(&trimmed) {
                                    history.push(trimmed.clone());
                                }
                                hist_idx = None;
                                draft.clear();
                                input.drain_to_string();
                                rows.push(Row::User(trimmed.clone()));
                                busy = true;
                                scroll_up = 0;
                                let _ = in_tx.send(Some(trimmed));
                            }
                        }
                    }
                    (KeyCode::Backspace, _) => {
                        if !busy {
                            input.backspace();
                        }
                    }
                    (KeyCode::Left, _) => {
                        if !busy {
                            input.move_left();
                        }
                    }
                    (KeyCode::Right, _) => {
                        if !busy {
                            input.move_right();
                        }
                    }
                    // Alt+Up/Down recall input history; Up/Down move the caret
                    // across lines inside the buffer.
                    (KeyCode::Up, KeyModifiers::ALT) => {
                        if !busy && !history.is_empty() {
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
                        if !busy {
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
                        if !busy {
                            input.move_up();
                        }
                    }
                    (KeyCode::Down, _) => {
                        if !busy {
                            input.move_down();
                        }
                    }
                    (KeyCode::Home, _) => {
                        if !busy {
                            input.home();
                        }
                    }
                    (KeyCode::End, _) => {
                        if !busy {
                            input.end();
                        }
                    }
                    (KeyCode::PageUp, _) => scroll_up = scroll_up.saturating_add(10),
                    (KeyCode::PageDown, _) => scroll_up = scroll_up.saturating_sub(10),
                    (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                        if !busy {
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
}
