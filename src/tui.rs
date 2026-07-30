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
    /// `collapsed` caches the preview at creation (like ToolCall does) so the
    /// renderer doesn't re-truncate the content every frame.
    ToolResult {
        name: String,
        content: String,
        collapsed: String,
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
    /// `/export [path]` — dump the conversation to a file (or clipboard when no
    /// path is given and a clipboard tool is available). The optional arg is the
    /// destination path, whitespace-trimmed.
    Export(Option<String>),
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
/// The command word is split from its argument on the first whitespace, so
/// `/export out.md` carries `Some("out.md")`.
fn classify(trimmed: &str) -> SlashCmd {
    let Some(rest) = trimmed.strip_prefix('/') else {
        return SlashCmd::Message;
    };
    let (word, raw_arg) = match rest.split_once(char::is_whitespace) {
        Some((w, a)) => (w, a.trim()),
        None => (rest.trim(), ""),
    };
    let arg = if raw_arg.is_empty() { None } else { Some(raw_arg.to_string()) };
    // Only /export takes an argument. After the classify refactor the other
    // commands silently dropped a trailing arg (`/model foo` ran /model with
    // "foo" ignored); reject them as unknown so the typo is surfaced. Unknown
    // carries the full `rest` so the message names what was actually typed.
    let stray_arg = arg.is_some();
    match word {
        "clear" if !stray_arg => SlashCmd::Clear,
        "export" => SlashCmd::Export(arg),
        "model" if !stray_arg => SlashCmd::Model,
        "limits" if !stray_arg => SlashCmd::Limits,
        "tools" if !stray_arg => SlashCmd::Tools,
        "system" if !stray_arg => SlashCmd::System,
        "help" if !stray_arg => SlashCmd::Help,
        _ => SlashCmd::Unknown(rest.to_string()),
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
        SlashCmd::Help => "commands: /clear /export /model /limits /tools /system /help".into(),
        SlashCmd::Unknown(name) => format!("[unknown command: /{name} — try /help]"),
        // Message/Clear/Export never reach here; empty keeps the match
        // exhaustive. Export and Clear have their own side-effecting arms in
        // the key handler.
        SlashCmd::Message | SlashCmd::Clear | SlashCmd::Export(_) => String::new(),
    }
}

/// Render the conversation rows as plain text for `/export` — a readable
/// transcript mirroring the on-screen layout, minus styling/wrapping. Empty
/// assistant rows (left while a turn is mid-stream) are skipped so the dump
/// has no bare "Agent" headers with no body.
fn render_transcript(rows: &[Row]) -> String {
    let mut out = String::new();
    for row in rows {
        match row {
            Row::User(t) => {
                out.push_str("You\n");
                out.push_str(t);
                out.push_str("\n\n");
            }
            Row::Assistant(t) if t.is_empty() => {}
            Row::Assistant(t) => {
                out.push_str("Agent\n");
                out.push_str(t);
                out.push_str("\n\n");
            }
            Row::ToolCall { name, input, .. } => {
                out.push_str(&format!("→ {name}: {input}\n\n"));
            }
            Row::ToolResult { name, content, is_error, .. } => {
                let tag = if *is_error { "✗" } else { "✓" };
                out.push_str(&format!("{tag} {name}: {content}\n\n"));
            }
            Row::System(t) => {
                out.push_str(&format!("· {t}\n\n"));
            }
        }
    }
    out
}

/// Best-effort copy to the system clipboard via the platform's CLI tool. Tries
/// each known tool in turn; returns true if one accepted the text. No clipboard
/// crate dep (keeps the binary lean) — this shelling-out covers macOS (pbcopy),
/// Wayland (wl-copy), X11 (xclip/xsel), and Windows (clip). A tool that isn't
/// installed fails to spawn and we move on; if none is present, `/export` falls
/// back to writing a file.
fn copy_to_clipboard(text: &str) -> bool {
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
        let exited_ok = child.wait().map(|s| s.success()).unwrap_or(false);
        let ok = fed && exited_ok;
        if ok {
            return true;
        }
    }
    false
}

/// Expand a leading `~/` (and a bare `~`) to `$HOME`, for `/export ~/notes.md`.
/// Only the current user's home — `~other` is left as-is (rare; not needed
/// here). `_with` takes home explicitly so the logic is unit-testable without
/// mutating the process-wide `$HOME`; `expand_tilde` binds it to the env.
fn expand_tilde_with(path: &str, home: Option<&std::ffi::OsStr>) -> String {
    let home = match home {
        Some(h) => h,
        None => return path.to_string(),
    };
    if path == "~" {
        return home.to_string_lossy().into_owned();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        let mut s = home.to_string_lossy().into_owned();
        s.push('/');
        s.push_str(rest);
        return s;
    }
    path.to_string()
}

fn expand_tilde(path: &str) -> String {
    expand_tilde_with(path, std::env::var_os("HOME").as_deref())
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
                    if let Some(w) = agent.push_user(vec![Block::Text(text)]) {
                        let _ = out_tx.send(UiMsg::Event(AgentEvent::Warning(w)));
                    }
                    if !run_once(&mut agent) {
                        return;
                    }
                }
                Input::Clear => {
                    if let Some(w) = agent.clear_messages() {
                        let _ = out_tx.send(UiMsg::Event(AgentEvent::Warning(w)));
                    }
                }
            }
        }
    });

    enable_raw_mode()?;
    // Create the restore guard IMMEDIATELY after the first state-changing
    // call, so a `?` failure in ANY of the steps below (EnterAlternateScreen /
    // EnableBracketedPaste / EnableMouseCapture / Terminal::new — e.g. stdout
    // not a tty, broken pipe) still drops the guard and restores the terminal.
    // Drop is fully idempotent (every disable is `let _ =`), so creating the
    // guard before some enables have run is harmless. (Previously the guard
    // was created after all four enables, leaving a mid-setup failure stranded
    // in raw mode + alt screen with no echo.)
    let _guard = TerminalGuard;
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
    // Name of the sub-agent whose text is currently accumulating into the
    // trailing Assistant row, so consecutive deltas from the same sub-agent
    // merge — and a different sub-agent (or the parent) starts a fresh row.
    let mut sub_streaming_name: Option<String> = None;
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
    // M6: memoize per-row markdown rendering so redraws (each streamed delta,
    // each scroll tick) don't re-parse the whole conversation. Cleared with
    // `rows` on `/clear`.
    let mut md_cache = MdCache::new();
    // Dirty flag for the render pass: set by any worker message or terminal
    // event; while clear (and not busy) the loop skips term.draw entirely.
    let mut dirty = true; // first frame must paint
    let mut last_draw = Instant::now();

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
                        dirty = true;
                        rows.push(Row::System(
                            "[worker stopped unexpectedly — press Esc to quit]".into(),
                        ));
                    }
                    break;
                }
            };
            dirty = true;
            match msg {
                UiMsg::Event(ev) => match ev {
                    AgentEvent::TurnStart(n) => {
                        turn = n;
                        streaming = false;
                    }
                    AgentEvent::TextDelta(t) => {
                        sub_streaming_name = None; // parent speaks: don't merge into a sub-agent row
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
                            collapsed: crate::agent::preview(&content),
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
                    AgentEvent::Warning(w) => {
                        rows.push(Row::System(format!("[warn] {w}")));
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
                    AgentEvent::SubAgent { name, event } => {
                        // Render the child's event tagged with its name so the
                        // user can see each sub-agent's progress (and tell
                        // parallel ones apart). Usage is already folded into the
                        // run totals at the agent level, so skip it here.
                        //
                        // Known limitation: consecutive TextDelta chunks merge
                        // into the last Assistant row whose tag matches `name`.
                        // If a turn runs two parallel sub-agents with the SAME
                        // name (the model gave identical `name`s), their text
                        // interleaves into one row. Distinguishing them would
                        // need a stable id on AgentEvent::SubAgent (not just the
                        // model-supplied name); the contract is that `name`
                        // uniquely identifies a sub-agent within a turn.
                        match &*event {
                            AgentEvent::TextDelta(t) => {
                                let same = sub_streaming_name.as_deref() == Some(name.as_str());
                                if !same {
                                    rows.push(Row::Assistant(format!("[{name}] ")));
                                    sub_streaming_name = Some(name.clone());
                                }
                                if let Some(Row::Assistant(buf)) = rows.last_mut() {
                                    buf.push_str(t);
                                }
                            }
                            AgentEvent::ToolCall { name: tn, input } => {
                                sub_streaming_name = None;
                                rows.push(Row::ToolCall {
                                    name: format!("[{name}] {tn}"),
                                    input: input.clone(),
                                    collapsed: compact(input),
                                    expanded: false,
                                });
                            }
                            AgentEvent::ToolResult {
                                name: tn,
                                is_error,
                                content,
                            } => {
                                sub_streaming_name = None;
                                rows.push(Row::ToolResult {
                                    collapsed: crate::agent::preview(content),
                                    name: format!("[{name}] {tn}"),
                                    content: content.clone(),
                                    is_error: *is_error,
                                    expanded: false,
                                });
                            }
                            AgentEvent::Done { .. } => sub_streaming_name = None,
                            AgentEvent::Warning(w) => {
                                sub_streaming_name = None;
                                rows.push(Row::System(format!("[{name}] [warn] {w}")));
                            }
                            _ => {} // TurnStart/Compressed/Usage/SubAgent: noisy or folded
                        }
                    }
                },
                UiMsg::Error(e) => {
                    rows.push(Row::System(format!("[error] {e}")));
                    busy = false;
                    // End every open stream: a leftover `streaming` /
                    // `sub_streaming_name` would let the NEXT run's first
                    // TextDelta merge into a stale row (or, when the last row
                    // is this System row, be silently dropped).
                    streaming = false;
                    sub_streaming_name = None;
                }
                UiMsg::RunFinished => {
                    busy = false;
                    streaming = false;
                    sub_streaming_name = None;
                }
            }
        }

        // Render — only when something changed (worker event drained, key or
        // mouse handled, first frame). While a run is in flight, also redraw
        // once a second so the status-bar clock keeps ticking. Idle frames
        // previously repainted the whole conversation at ~16fps for nothing.
        let clock_tick = busy && last_draw.elapsed() >= Duration::from_secs(1);
        if dirty || clock_tick {
            dirty = false;
            last_draw = Instant::now();
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
                        // M6: reuse the cached render unless this row's text
                        // grew since last frame (streaming). Prior rows and
                        // scroll redraws hit the cache; only the in-flight row
                        // (or a first render) re-parses markdown.
                        let rendered = md_cache.get_or_render(idx, t, |txt| {
                            render_markdown(txt).iter().map(line_to_owned).collect()
                        });
                        lines.extend(rendered.iter().cloned());
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
                        collapsed,
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
                            collapsed.clone()
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
            // status bar or off-screen. The same applies HORIZONTALLY: the
            // Paragraph doesn't wrap, so a line wider than the box is clipped
            // and an unclamped caret would be drawn past the border, over the
            // conversation area (or off-screen on a narrow terminal).
            let inner_rows = (input_h as usize).saturating_sub(2);
            let input_scroll = crow
                .saturating_sub(inner_rows.saturating_sub(1))
                .min(u16::MAX as usize) as u16;
            let inner_cols = (chunks[1].width as usize).saturating_sub(2).max(1);
            let lead = if crow == 0 { prompt.width() } else { 0 };
            let caret_col = ccol + lead; // display column within the caret's line
            let (hscroll, caret_x) = input_hscroll(caret_col, inner_cols);
            let input_widget = Paragraph::new(format!("{prompt}{body}"))
                .block(WBlock::default().borders(Borders::ALL).title("input"))
                .scroll((input_scroll, hscroll));
            f.render_widget(input_widget, chunks[1]);
            // Place the caret. Column = border(1) + display width of the text
            // before the caret (+ the prompt, on the first line only), minus
            // the horizontal scroll. Display width keeps it correct for wide
            // chars; hscroll keeps it inside the box on over-wide lines.
            if !busy && mode == Mode::Input {
                let x = chunks[1].x + 1 + caret_x;
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
                Span::raw("  Ctrl-C cancel · Tab browse · Esc quit · PgUp/PgDn·wheel · Shift+drag copy · /export"),
            ]);
            f.render_widget(Paragraph::new(status), chunks[2]);
            })?;
        }

        // Keyboard. Poll longer when idle: a keypress wakes the poll
        // immediately, so the longer timeout costs no input latency but
        // avoids spinning the loop at ~16fps while nothing is happening.
        let poll_ms = if busy { 60 } else { 250 };
        if event::poll(Duration::from_millis(poll_ms))? {
            let ev = event::read()?;
            // Any event may change the view (scroll, caret, mode, …).
            dirty = true;
            if let Event::Key(key) = ev {
                // Accept key repeats too: on Windows, crossterm reports held
                // keys as Press once then Repeat — filtering to Press only
                // makes held Backspace/arrows delete/move a single char.
                // Release events are still ignored (they'd double-fire).
                if key.kind == KeyEventKind::Release {
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
                            sub_streaming_name = None;
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
                                        scroll_up = 0;
                                        // Deliver before flipping busy. If the
                                        // worker is gone (panicked mid-run in a
                                        // debug build, or tearing down) the send
                                        // fails — don't leave the UI stuck in
                                        // "running" waiting for a RunFinished that
                                        // will never arrive. The out_rx disconnect
                                        // path is a backstop, but it only fires on
                                        // a later frame's drain; handling the send
                                        // result here is immediate and doesn't
                                        // rely on the loop reaching that drain.
                                        if in_tx.send(Some(Input::Text(trimmed))).is_err() {
                                            busy = false;
                                            // The message never reached the agent — drop it
                                            // from the transcript so the on-screen history
                                            // matches what the agent actually saw (otherwise it
                                            // reads as if the agent ignored the user).
                                            if matches!(rows.last(), Some(Row::User(_))) {
                                                rows.pop();
                                            }
                                            rows.push(Row::System(
                                                "[worker stopped — message not delivered; \
                                                 press Esc to quit]"
                                                    .into(),
                                            ));
                                        } else {
                                            busy = true;
                                        }
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
                                        md_cache.clear();
                                        in_tokens = 0;
                                        out_tokens = 0;
                                        cache_create = 0;
                                        cache_read = 0;
                                        // Reset Nav state: select_idx pointed at
                                        // a now-vanished row, and clearing is an
                                        // input-mode action.
                                        select_idx = None;
                                        mode = Mode::Input;
                                        // Send before claiming success. If the worker is gone
                                        // (panicked mid-run in a debug build, or tearing down)
                                        // the send fails and the agent-side history is NOT
                                        // cleared — report that honestly, mirroring the text
                                        // path, instead of asserting "[history cleared]". The
                                        // local UI reset above still applies (the user asked to
                                        // clear the screen regardless).
                                        let cleared = in_tx.send(Some(Input::Clear)).is_ok();
                                        rows.push(Row::System(
                                            if cleared {
                                                "[history cleared]".into()
                                            } else {
                                                "[worker stopped — UI cleared but session \
                                                 history not cleared]"
                                                    .into()
                                            },
                                        ));
                                    }
                                    SlashCmd::Export(arg) => {
                                        // The mouse-capture that enables wheel
                                        // scroll also intercepts click-drag, so
                                        // there's no terminal-agnostic way to
                                        // select the on-screen output. /export
                                        // is the app-level escape hatch: dump the
                                        // conversation as text. With a path →
                                        // that file; without → clipboard if a
                                        // tool is available, else a default file.
                                        input.drain_to_string();
                                        scroll_up = 0;
                                        let n = rows.len();
                                        let text = render_transcript(&rows);
                                        let msg = match arg {
                                            Some(path) => {
                                                use std::fs::OpenOptions;
                                                use std::io::Write;
                                                // Expand `~/` so /export ~/notes.md lands in the
                                                // home dir, not a literal "~/" directory.
                                                // create_new refuses to clobber an existing file:
                                                // the TUI has no undo and the success line gave
                                                // no warning, so a typo'd path (e.g. ~/.zshrc)
                                                // would otherwise truncate real content silently.
                                                // (The clipboard-less fallback below keeps
                                                // overwriting its own scratch file on purpose.)
                                                let dest = expand_tilde(&path);
                                                let res = OpenOptions::new()
                                                    .write(true)
                                                    .create_new(true)
                                                    .open(&dest)
                                                    .and_then(|mut f| f.write_all(text.as_bytes()));
                                                match res {
                                                    Ok(_) => format!("[exported {n} rows to {dest}]"),
                                                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                                                        format!(
                                                            "[export to {dest} failed: file already \
                                                             exists — /export won't overwrite; remove \
                                                             it or choose another path]"
                                                        )
                                                    }
                                                    Err(e) => format!("[export to {dest} failed: {e}]"),
                                                }
                                            }
                                            None => {
                                                if copy_to_clipboard(&text) {
                                                    format!("[copied {n} rows to clipboard]")
                                                } else {
                                                    let path = "tinyagent-export.md";
                                                    match std::fs::write(path, &text) {
                                                        Ok(_) => format!(
                                                            "[no clipboard tool found; \
                                                             exported {n} rows to {path}]"
                                                        ),
                                                        Err(e) => {
                                                            format!("[export failed: {e}]")
                                                        }
                                                    }
                                                }
                                            }
                                        };
                                        rows.push(Row::System(msg));
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

/// Horizontal scroll for the input box: returns `(scroll_offset, caret_x)`
/// where `caret_x` is the caret's on-screen column relative to the box's
/// inner left edge, guaranteed `< inner_cols`. The input Paragraph doesn't
/// wrap, so a line wider than the box must be horizontally scrolled to keep
/// the caret visible — an unclamped caret would be drawn past the border,
/// over the conversation area (or off-screen on a narrow terminal).
fn input_hscroll(caret_col: usize, inner_cols: usize) -> (u16, u16) {
    let inner_cols = inner_cols.max(1);
    let scroll = caret_col
        .saturating_sub(inner_cols.saturating_sub(1))
        .min(u16::MAX as usize) as u16;
    let x = caret_col
        .saturating_sub(scroll as usize)
        .min(inner_cols.saturating_sub(1))
        .min(u16::MAX as usize) as u16;
    (scroll, x)
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

/// Snapshot a `Line` with every span's borrowed content materialized into an
/// owned `String` → `Line<'static>`. Style and alignment are preserved (both
/// `Copy`). M6's render cache stores owned lines so it outlives the row text it
/// was rendered from (which is borrowed during the draw closure).
fn line_to_owned(line: &Line) -> Line<'static> {
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

/// Per-row cache of rendered-markdown lines (M6). The draw closure re-renders
/// every assistant message on EVERY frame — including each streamed TextDelta
/// and each scroll tick — which re-parses the whole conversation's markdown via
/// `tui_markdown::from_str` each time. That's O(history) per event and gets
/// slow for long chats. This memoizes the parse keyed by row index, invalidated
/// only when that row's text length changes (an assistant row grows while
/// streaming; once final it's stable). So a streaming delta re-renders only the
/// in-flight row, and scrolling/history redraws hit the cache for every prior
/// row. Safe because `rows` is append-only except `rows.clear()` (which clears
/// this cache too) — indices are stable for a row's lifetime.
struct MdCache {
    entries: std::collections::HashMap<usize, (usize, Vec<Line<'static>>)>,
}

impl MdCache {
    fn new() -> Self {
        Self {
            entries: std::collections::HashMap::new(),
        }
    }
    fn clear(&mut self) {
        self.entries.clear();
    }
    /// Cached rendered lines for row `idx`, recomputing via `render` on a miss
    /// (absent, or the text length changed since the last render). The borrow
    /// of the returned slice ends at the next `&mut self` call.
    fn get_or_render(
        &mut self,
        idx: usize,
        text: &str,
        render: impl FnOnce(&str) -> Vec<Line<'static>>,
    ) -> &[Line<'static>] {
        // Stale when absent OR when the text length changed since the last
        // render (an assistant row grows while streaming; once final it's a
        // stable length, so every scroll/redraw hits the cache).
        let stale = self
            .entries
            .get(&idx)
            .is_none_or(|(len, _)| *len != text.len());
        if stale {
            self.entries.insert(idx, (text.len(), render(text)));
        }
        &self.entries[&idx].1
    }
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
    /// paste. `\n` splits into a fresh line; the CR of a CRLF pair (Windows
    /// pastes) is dropped so it doesn't leave a stray CR in the input. A lone
    /// `\r` is kept — swallowing every CR would corrupt content that uses it
    /// deliberately (e.g. progress-redraw snippets).
    fn insert_str(&mut self, s: &str) {
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\n' => self.newline(),
                '\r' if chars.peek() == Some(&'\n') => {}
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

        // A LONE \r is kept: only the CR of a CRLF pair is dropped — content
        // that uses \r deliberately must survive the paste.
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
        // /export takes an optional path arg, split off + trimmed.
        assert_eq!(classify("/export"), SlashCmd::Export(None));
        assert_eq!(classify("/export out.md"), SlashCmd::Export(Some("out.md".into())));
        assert_eq!(classify("/export   a/b.txt  "), SlashCmd::Export(Some("a/b.txt".into())));
        // /export with only trailing spaces → no arg.
        assert_eq!(classify("/export "), SlashCmd::Export(None));
        // No-arg commands reject a trailing arg (only /export takes one):
        // `/model foo` is a typo, not a silently-ignored /model.
        assert_eq!(classify("/model foo"), SlashCmd::Unknown("model foo".into()));
        assert_eq!(classify("/clear x"), SlashCmd::Unknown("clear x".into()));
    }

    #[test]
    fn expand_tilde_resolves_home() {
        let home = std::ffi::OsStr::new("/Users/x");
        assert_eq!(expand_tilde_with("~/notes.md", Some(home)), "/Users/x/notes.md");
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
        // on-screen layout (User/Agent/tool rows), skips mid-stream empty
        // assistant rows, and tags error results distinctly.
        let rows = vec![
            Row::User("hello".into()),
            Row::Assistant(String::new()), // empty → skipped (no bare "Agent")
            Row::Assistant("I am fine".into()),
            Row::ToolCall {
                name: "shell".into(),
                input: serde_json::Value::Null,
                collapsed: "null".into(),
                expanded: false,
            },
            Row::ToolResult {
                name: "shell".into(),
                content: "a.txt".into(),
                collapsed: "a.txt".into(),
                is_error: false,
                expanded: false,
            },
            Row::ToolResult {
                name: "shell".into(),
                content: "boom".into(),
                collapsed: "boom".into(),
                is_error: true,
                expanded: false,
            },
            Row::System("[note]".into()),
        ];
        let out = render_transcript(&rows);
        assert!(out.contains("You\nhello"), "user row: {out:?}");
        assert!(out.contains("Agent\nI am fine"), "assistant body: {out:?}");
        assert!(!out.contains("Agent\n\n"), "empty assistant must be skipped");
        assert!(out.contains("→ shell: null"), "tool call: {out:?}");
        assert!(out.contains("✓ shell: a.txt"), "tool result: {out:?}");
        assert!(out.contains("✗ shell: boom"), "error result tagged: {out:?}");
        assert!(out.contains("· [note]"), "system row: {out:?}");
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
                collapsed: "c".into(),
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

    // ── M6: render cache + owned-line conversion ───────────────────────

    #[test]
    fn input_hscroll_keeps_caret_inside_the_box() {
        // Short line: no scroll, caret at its natural column.
        assert_eq!(input_hscroll(3, 10), (0, 3));
        // Caret exactly at the last inner column: still no scroll.
        assert_eq!(input_hscroll(9, 10), (0, 9));
        // Caret past the right edge: scrolls just enough to show it on the
        // last inner column — never beyond the border.
        assert_eq!(input_hscroll(10, 10), (1, 9));
        assert_eq!(input_hscroll(500, 10), (491, 9));
        // Degenerate widths can't underflow or place the caret outside.
        assert_eq!(input_hscroll(0, 0), (0, 0));
        assert_eq!(input_hscroll(42, 1), (42, 0));
        // Invariant for a sweep of values: caret_x < inner_cols always.
        for caret in 0..300usize {
            for w in 1..20usize {
                let (_, x) = input_hscroll(caret, w);
                assert!((x as usize) < w, "caret {caret} at width {w} → x {x}");
            }
        }
    }

    #[test]
    fn input_box_horizontal_scroll_renders_caret_line_window() {
        // Widget level (the project's TestBackend convention): with the scroll
        // from input_hscroll applied, the visible window of an over-wide line
        // contains the text around the caret, not the line's clipped head.
        use ratatui::backend::{Backend, TestBackend};
        use ratatui::layout::Rect;

        let body = "abcdefghijklmnopqrst"; // 20 cols, box shows 6
        let (scroll, x) = input_hscroll(20, 6); // caret at end
        assert_eq!(x, 5);
        let area = Rect::new(0, 0, 8, 3);
        let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        term.draw(|f| {
            let p = Paragraph::new(body)
                .block(WBlock::default().borders(Borders::ALL))
                .scroll((0, scroll));
            f.render_widget(p, area);
            f.set_cursor_position((1 + x, 1));
        })
        .unwrap();
        let buf = term.backend().buffer();
        let row: String = (1..7).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        // Caret at end-of-line (col 20): window shows the tail + the blank
        // caret cell after 't' — the clipped head "abcd…" is scrolled away.
        assert_eq!(row, "pqrst ", "scrolled window: {row:?}");
        // The drawn cursor stays within the box's inner area.
        let pos = term.backend_mut().get_cursor_position().unwrap();
        assert!(pos.x >= 1 && pos.x <= 6, "cursor x {} outside inner area", pos.x);
    }

    #[test]
    fn md_cache_recomputes_only_when_text_changes() {
        // M6: the draw closure re-renders every assistant row each frame. The
        // cache must reuse a row's render across frames (scrolling, other rows
        // streaming) and only recompute when THAT row's text grew. We prove
        // reuse by changing the renderer between calls: a cache hit ignores the
        // new renderer (returns the old content), a miss uses it.
        let mut c = MdCache::new();
        let upper = |t: &str| vec![Line::from(t.to_string())];
        let lower = |t: &str| vec![Line::from(t.to_string())];

        // Miss → upper renderer used.
        let r = c.get_or_render(0, "hi", upper);
        assert_eq!(r[0].spans.first().unwrap().content.as_ref(), "hi");

        // Hit (same idx+text): the lower renderer is IGNORED — still "hi", not
        // "hi" via lower (here identical, so swap to a distinguishing renderer).
        let distinguishing = |t: &str| vec![Line::from(t.to_string() + "!")];
        let r = c.get_or_render(0, "hi", distinguishing);
        assert_eq!(
            r[0].spans.first().unwrap().content.as_ref(),
            "hi",
            "cache hit must NOT re-render (distinguishing renderer ignored)"
        );

        // Text grew (streaming): miss → distinguishing renderer now applied.
        let r = c.get_or_render(0, "hi there", distinguishing);
        assert_eq!(r[0].spans.first().unwrap().content.as_ref(), "hi there!");

        // A different row is independent (its own miss).
        let r = c.get_or_render(7, "hi", upper);
        assert_eq!(r[0].spans.first().unwrap().content.as_ref(), "hi");
        // Row 0 untouched by row 7's render.
        let r = c.get_or_render(0, "hi there", distinguishing);
        assert_eq!(r[0].spans.first().unwrap().content.as_ref(), "hi there!");

        // clear() drops entries → next access is a miss.
        c.clear();
        let r = c.get_or_render(0, "hi there", lower);
        assert_eq!(
            r[0].spans.first().unwrap().content.as_ref(),
            "hi there",
            "clear must force a re-render with the new renderer"
        );
    }

    #[test]
    fn line_to_owned_preserves_spans_style_and_alignment() {
        // M6: the cache stores owned lines; the conversion must keep every
        // span's text + style and the line's alignment, materializing the
        // (possibly borrowed) content into owned Strings.
        use ratatui::layout::Alignment;
        let src = Line::from(vec![
            Span::styled("hello", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
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
        let borrowed: Vec<Line> = render_markdown(md);
        let owned: Vec<Line<'static>> =
            render_markdown(md).iter().map(line_to_owned).collect();

        // Wide enough that the content isn't wrapped (we're comparing caches,
        // not wrapping). Render both into equal buffers and flatten cells.
        let area = Rect::new(0, 0, 40, 5);
        let cells_of = |lines: Vec<Line>| -> String {
            let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            term.draw(|f| f.render_widget(Paragraph::new(lines), area)).unwrap();
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
        assert!(rendered.contains('标'), "CJK header glyph missing: {rendered:?}");
    }
}
