//! Session wiring: `run` (the entry point main.rs calls), the worker thread,
//! the event/input channels, the terminal guards, and `ui_loop` — the
//! event-handling session loop that ties the other modules together.

use super::input::{history_down, history_up, input_hscroll, up_is_history, InputBuffer};
use super::markdown::{ansi_plain, MdCache};
use super::render::{
    build_input_title, build_status_line, chrome_fg, commit_rows, content_width, copy_to_clipboard,
    render_zone_tail, render_zone_top, strip_controls, strip_controls_json, welcome_text,
    wrap_history,
};
use super::slash::{
    classify, popup_key, render_context_report, slash_popup_lines, slash_reply, PopupAction,
    SlashCmd, SlashPopup,
};
use super::theme::{
    dim_fg, fmt_ctx, fmt_tokens, set_theme_from_env, AGENT_FG, TOOL_CALL_FG, USER_FG,
};
use super::transcript::{
    append_delta, append_sub_delta, append_thinking_delta, auto_expand_result, build_history_lines,
    code_log_row, commit_pending_end, compact, drop_queued_ghosts, inflight_row_index,
    nth_last_tool_result, remove_oldest_queued, render_transcript, split_streaming_row,
    tool_elapsed_for, tool_running_line, LiveRow, Row, ToolRunning, ROLE_BODY_INDENT,
};
use crate::agent::{Agent, AgentEvent};
use crate::config::Config;
use crate::provider::Block;
use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::ExecutableCommand;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as WBlock, BorderType, Borders, Paragraph};
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::io::stdout;
use std::sync::mpsc;
use std::time::{Duration, Instant};
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
const INPUT_PLACEHOLDER: &str = "Message TinyAgent… (/help)";

/// How long a first idle Ctrl-C arms the quit confirmation. A single press
/// in idle only shows the "press again" notice — the busy-Ctrl-C muscle
/// memory firing right after a run ends must not kill the session (the same
/// accident that got Esc-as-quit removed).
const CTRL_C_QUIT_WINDOW: Duration = Duration::from_secs(2);

/// Whether a second idle Ctrl-C (at `now`) confirms the quit. None armed (or
/// past the window) means this press only arms.
pub(super) fn ctrl_c_should_quit(armed: Option<Instant>, now: Instant) -> bool {
    armed.is_some_and(|t| now.duration_since(t) <= CTRL_C_QUIT_WINDOW)
}

/// Stop reasons that mean "the turn ended normally", across both provider
/// families (Anthropic: end_turn / tool_use / stop_sequence; OpenAI: stop /
/// tool_calls). Only abnormal stops (max_tokens / length / error / unknown)
/// get a System row — a routine end needs no noise. `cancelled` is here too:
/// the Esc/Ctrl-C handler already printed `[cancelling…]`, so a second
/// `[done: cancelled]` line would just duplicate the notice.
pub(super) fn is_normal_stop(reason: &str) -> bool {
    matches!(
        reason,
        "end_turn" | "tool_use" | "stop" | "tool_calls" | "stop_sequence" | "cancelled"
    )
}

/// Expand a leading `~/` (and a bare `~`) to `$HOME`, for `/export ~/notes.md`.
/// Only the current user's home — `~other` is left as-is (rare; not needed
/// here). `_with` takes home explicitly so the logic is unit-testable without
/// mutating the process-wide `$HOME`; `expand_tilde` binds it to the env.
pub(super) fn expand_tilde_with(path: &str, home: Option<&std::ffi::OsStr>) -> String {
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

pub(super) enum UiMsg {
    Event(AgentEvent),
    Error(String),
    RunFinished,
    /// `/context`: the worker's `context_breakdown()` snapshot, rendered by
    /// the UI loop (which owns the config the window line reads).
    Context(crate::context_report::ContextBreakdown),
    /// Kernel diagnostics (MCP background-reader warnings, session-recovery
    /// notes, …) forwarded from the process-global diag sink by
    /// [`install_diag_bridge`]. Without this the kernel's fallback `eprintln`
    /// fires from whatever thread hit the warning and paints straight over
    /// the raw-mode viewport (and the native scrollback), where it stays
    /// until the next redraw — the redraw gate is `dirty || (busy && tick)`,
    /// so in idle the garbage frame lingers indefinitely. The message text
    /// is self-describing ("[warn] …"), so the level only picks the row's
    /// color; the sink drops the send when the UI thread is already gone.
    Diag(tiny_agent::diag::DiagLevel, String),
}

/// Control or text input from the UI to the worker thread. `None` on the
/// channel means "quit" (Esc / idle Ctrl-C); `Some(Text)` is a user message;
/// `Some(Clear)` is the `/clear` command, which the worker applies to the
/// agent's history without running a turn; `Some(Context)` asks for the
/// breakdown snapshot (read-only, no turn).
pub(super) enum Input {
    Text(String),
    Clear,
    Context,
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

/// Restores the terminal on drop. Installed right after raw mode is entered,
/// so a panic (or a propagated `?` error) inside the UI loop unwinds through
/// this and still leaves the user's terminal usable — without it, a panic
/// mid-draw strands them in raw mode with no echo. Inline mode lives on the
/// PRIMARY screen (no alternate screen) and captures no mouse, so teardown is
/// just bracketed-paste + raw-mode — native selection/scroll keep working.
/// Drop also clears the diag sink: the sink is process-global, so the
/// install (in `run`, right after this guard is created) and this clear stay
/// symmetric on EVERY exit path — normal quit, early `?` failure, and panic
/// unwind all drop the guard. A sink left installed after the TUI is gone
/// would keep sending into a dead channel (harmless) — worse, a later
/// non-TUI phase of the process would silently swallow diagnostics that its
/// stderr fallback was supposed to print.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = stdout().execute(DisableBracketedPaste);
        let _ = disable_raw_mode();
        tiny_agent::diag::clear_diag_sink();
    }
}

/// Install the TUI's diag bridge: every kernel `diag_warn!` / `diag_info!`
/// (from ANY thread — the MCP readers chief among them) is packaged as a
/// [`UiMsg::Diag`] on the UI event channel instead of falling back to
/// `eprintln`, which would write from a foreign thread onto the raw-mode
/// terminal. Split out of `run` as a named function so the bridge itself is
/// testable without a terminal (see `diag_sink_forwards_kernel_diag_to_ui`).
/// Pair with [`TerminalGuard`]'s `clear_diag_sink` on teardown.
pub(super) fn install_diag_bridge(out_tx: mpsc::Sender<UiMsg>) {
    tiny_agent::diag::install_diag_sink(Box::new(move |level, msg| {
        // A failed send means the UI thread has exited; the message is
        // dropped — the alternative (falling back to eprintln) would reprint
        // the exact raw-mode garbage this bridge exists to prevent.
        let _ = out_tx.send(UiMsg::Diag(level, msg.to_owned()));
    }));
}

pub fn run(
    agent: Agent,
    cfg: &Config,
    initial_task: Option<String>,
    auto_start: bool,
    resume_rows: Vec<Row>,
) -> Result<()> {
    // Bind the palette knob before any rendering reads dim_fg()/code_fg().
    set_theme_from_env(std::env::var("TINYAGENT_TUI_THEME").ok());
    // UI → worker: user inputs (None = quit).
    let (in_tx, in_rx) = mpsc::channel::<Option<Input>>();
    // Worker → quit-path: the worker signals it has exited, so quitting can
    // join within a bounded wait instead of detaching mid-tool.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    // Worker → UI: agent events.
    let (out_tx, out_rx) = mpsc::channel::<UiMsg>();
    // Cancellation: Esc sets this; the agent checks it at each turn boundary.
    // This is the RECOVERABLE per-turn interrupt — the next turn reuses the same
    // MCP server child, so it must never tear it down.
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Teardown: set ONLY on session end (the quit path below), never on a
    // per-turn Esc. The MCP shutdown-reaper watches this — firing it kills the
    // server child so a stuck call unblocks on quit. Split from `cancel` so a
    // recoverable turn-interrupt doesn't irreversibly kill the MCP server the
    // next turn couldn't restart (P1 audit fix).
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Set up the terminal BEFORE spawning the worker. The worker may fire an
    // initial task immediately (agent.ends_with_user() → run_once), and every
    // step below can fail with `?` (stdout not a tty, broken pipe, Terminal
    // creation). When setup preceded the spawn, such a failure returned early
    // and skipped the cancel/join cleanup below — leaving the agent running API
    // calls and tools in the background with no UI and no cancel signal. Doing
    // this first means a setup failure simply returns before any worker exists.
    enable_raw_mode()?;
    // Create the restore guard IMMEDIATELY after the first state-changing
    // call, so a `?` failure in ANY of the steps below (EnableBracketedPaste /
    // Terminal::with_options — e.g. stdout not a tty, broken pipe) still drops
    // the guard and restores the terminal. Drop is fully idempotent (every
    // disable is `let _ =`), so creating the guard before some enables have
    // run is harmless.
    let _guard = TerminalGuard;
    // Take over the kernel's diag channel for as long as raw mode is active:
    // kernel warnings (the MCP background readers most prominently) must
    // reach the transcript as UiMsg::Diag rows rendered by the UI thread,
    // not eprintln from a foreign thread straight onto the raw-mode screen.
    // The guard above clears the sink on every teardown path, keeping the
    // global install/clear symmetric.
    install_diag_bridge(out_tx.clone());
    // Bracketed paste: terminals that support it deliver a pasted blob as one
    // Event::Paste (newlines included) instead of replaying it as keystrokes —
    // which would hit the Enter handler and send each line as a message.
    stdout().execute(EnableBracketedPaste)?;
    // Inline viewport: the TUI lives in a fixed INLINE_HEIGHT-row band at the
    // bottom of the PRIMARY screen — no alternate screen, no mouse capture.
    // Finalized conversation turns are committed into the terminal's own
    // scrollback via Terminal::insert_before, so the host terminal's NATIVE
    // text selection, scrolling, and search all work on the conversation. The
    // float-on-redraw selection bug of the full-screen model cannot occur
    // because the conversation lives in real scrollback, not a redrawn frame.
    let mut term = Terminal::with_options(
        ratatui::backend::CrosstermBackend::new(stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(INLINE_HEIGHT),
        },
    )?;

    // The worker thread owns the agent and drives run() per user input.
    let worker_cancel = cancel.clone();
    let worker_shutdown = shutdown.clone();
    let worker = std::thread::spawn(move || {
        // Signal completion on ANY exit (normal loop end or early return) via
        // Drop, so the quit path can join within a bounded wait.
        let _done = DoneSignal(done_tx);
        let mut agent = agent;
        agent.set_cancel(worker_cancel);
        // Arm the MCP shutdown-reaper on the session-end flag (NOT cancel): a
        // per-turn Esc leaves the server alive; only a real quit kills it.
        agent.set_shutdown(worker_shutdown);

        // Run one agent turn. Returns false once the UI has gone away
        // (out_rx dropped) so the loop can stop instead of pushing into
        // a dead channel.
        let run_once = |agent: &mut Agent| -> bool {
            // Acceptance point (cancel contract, TinyWorker): run() never
            // clears the flag, so the party accepting the new task does.
            // This closure IS that point — it fires once per submitted user
            // input (and for the initial/resumed auto-run). Without this, an
            // Esc that cancelled the previous turn would instantly cancel
            // every following one. Idle Esc never touches the flag, so the
            // clear cannot swallow a stop the user just signalled.
            agent.clear_cancel();
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
                Input::Context => {
                    // Read-only snapshot; the anchor-derived calibration is
                    // computed under the worker's exclusive hold of the agent.
                    let _ = out_tx.send(UiMsg::Context(agent.context_breakdown()));
                }
            }
        }
    });

    let result = ui_loop(
        &mut term,
        cfg,
        &in_tx,
        &out_rx,
        &cancel,
        initial_task.as_deref(),
        auto_start,
        resume_rows,
    );

    // Stop the worker before restoring the terminal. If terminal restoration
    // errors out below, the worker has already been told to stop (and is
    // detached on scope exit) instead of running unchecked to its next limit.
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    // Session end: fire the SEPARATE teardown flag so the MCP shutdown-reaper
    // kills a wedged server child and unblocks the worker's bounded join below.
    // (cancel alone won't: the worker may be stuck inside a 120s MCP call, not
    // at a turn boundary where cancel is checked.) This is the ONLY place
    // shutdown is raised — a per-turn Esc never reaches here.
    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(in_tx); // signal worker to exit
                 // Wait a bounded time for the worker to actually exit. A cancel-aware
                 // shell kills its process group within ~50ms, so joining here prevents the
                 // orphan-on-quit case where the process exits mid-tool and reparents a
                 // running shell to init. A wedged MCP call is now killed by the shutdown
                 // reaper armed above (child killed → stdout EOF → blocked request returns)
                 // within the reaper's 100ms poll, so it too unblocks here. A genuinely
                 // non-cancel-aware tool with no teardown just lets this time out — we then
                 // detach as before, never hanging exit.
    let worker_stuck = done_rx.recv_timeout(Duration::from_secs(3)).is_err();
    drop(worker); // detach if the worker is still stuck past the deadline
                  // Restore the terminal BEFORE reporting discarded queued messages — a note
                  // printed while raw mode is still active would be garbled. The guard's Drop
                  // is idempotent, so a panic path below still ends clean. Inline mode leaves
                  // the committed scrollback intact: the conversation stays in the terminal's
                  // history like ordinary command output.
    drop(_guard);
    // Step past the inline viewport so the dropped-queue note (and the shell
    // prompt on return) doesn't overwrite the viewport's last frame.
    println!();
    // A detached worker is a lifecycle fact the user must see, not silence:
    // a third-party tool / host callback with no cancel awareness can still
    // be running (writing files, holding connections) after the terminal is
    // back. The MCP reaper + cancel-aware tools normally finish within the
    // 3s join above; this fires only for the remainder.
    if worker_stuck {
        eprintln!(
            "[tiny-agent] worker did not exit within 3s — detached; a non-cancel-aware \
             tool may still be running in the background"
        );
    }
    if let Ok(dropped) = result {
        if dropped > 0 {
            eprintln!("[tiny-agent] {dropped} queued message(s) discarded on quit");
        }
    }
    result.map(|_| ())
}

// The loop state is threaded as plain locals (rows/queued/history/…); the
// parameters are exactly the pieces owned by the caller. Grouping them into
// a struct would only shuffle the same fields to one more place.
#[allow(clippy::too_many_arguments)]
fn ui_loop(
    term: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    cfg: &Config,
    in_tx: &mpsc::Sender<Option<Input>>,
    out_rx: &mpsc::Receiver<UiMsg>,
    cancel: &std::sync::atomic::AtomicBool,
    initial_task: Option<&str>,
    auto_start: bool,
    resume_rows: Vec<Row>,
) -> Result<usize> {
    // The full conversation, in append order. Finalized prefixes are committed
    // into the terminal scrollback via insert_before and never re-rendered; the
    // live viewport only ever shows the tail end (the in-flight streaming row
    // and queued ghosts), so memory grows with the session but per-frame work
    // does not.
    let mut rows: Vec<Row> = Vec::new();
    // Watermark: rows[0..committed] have already been flushed to scrollback and
    // are permanent (only Queued ghosts are ever removed, and they live in the
    // uncommitted tail). See `pending_end` for the finalize rule.
    let mut committed: usize = 0;
    let mut input = InputBuffer::new();
    let mut busy = false;
    // Messages typed while a run is in flight, oldest first. Enter queues
    // here instead of sending; the RunFinished/Error flush pops one and sends
    // it (only then committing its Row::User, so the transcript stays in true
    // send order).
    let mut queued: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    // Start of the current run (one user message → possibly many turns), for
    // the status-bar elapsed time and the spinner frame. None when idle.
    let mut run_start: Option<Instant> = None;
    // What the agent is doing right now ("thinking" / "writing" / "→ tool"),
    // shown in the busy status segment; fed by the event stream.
    let mut activity = String::new();
    let mut streaming = false;
    // Transient "agent is thinking" marker: set at TurnStart (and when a
    // queued message is flushed into a new run), cleared by the first visible
    // output (text delta or tool call) and at run end. Render-only — the live
    // viewport renders a `● …` line from it; nothing is committed for it.
    let mut thinking = false;
    // Transient in-flight tool tracker (render-only, like `thinking`): the
    // ToolCall row commits immediately, so this placeholder is what keeps the
    // live zone showing WHICH tool is running and for how long.
    let mut tool_running = ToolRunning::new();
    // Name of the sub-agent whose text is currently accumulating into the
    // trailing Assistant row, so consecutive deltas from the same sub-agent
    // merge — and a different sub-agent (or the parent) starts a fresh row.
    let mut sub_streaming_name: Option<String> = None;
    // Tag of the reasoning trace currently accumulating into a trailing
    // Row::Thinking ("" for the parent, the sub-agent's name for a child);
    // None when no trace is in flight. Same lifecycle as `streaming`.
    let mut thinking_stream: Option<String> = None;
    // Input history (Alt+Up/Down to recall). `draft` holds the in-progress
    // text when the user starts browsing, so Down past the newest restores it.
    let mut history: Vec<String> = Vec::new();
    let mut hist_idx: Option<usize> = None;
    let mut draft: String = String::new();
    // Slash-command completion popup: refreshed from the input text before
    // every draw, consulted by the key handler while open.
    let mut popup = SlashPopup::new();
    // First idle Ctrl-C timestamp: a second press within CTRL_C_QUIT_WINDOW
    // actually quits; the first only shows the "press again" notice.
    let mut ctrl_c_armed: Option<Instant> = None;
    // Full cwd path for the status bar's right-aligned segment — computed
    // once, not per frame.
    let cwd_path: String = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Running token totals for the session, summed from each turn's usage.
    let mut in_tokens: u64 = 0;
    let mut out_tokens: u64 = 0;
    // Prompt-cache write/read totals — shown in the status bar so the user can
    // see caching paying off (Anthropic reports both; OpenAI reports reads).
    let mut cache_create: u64 = 0;
    let mut cache_read: u64 = 0;
    // Estimated context-window occupancy after the latest API call, SET (not
    // summed) from each Usage event: prompt tokens (incl. cached parts) plus
    // the response just appended to the history. Drives the status bar's
    // `ctx` segment.
    let mut ctx_tokens: u64 = 0;
    // The agent's current task plan, tracked via the `todo` tool. Shown live in
    // the status bar (a `plan done/total` segment) so progress is visible
    // without scrolling — the full checklist also lives in the most recent
    // `todo` tool-result row in the transcript. Updated on each TodoUpdate
    // (full-replacement semantics: each call sends the entire plan).
    let mut plan: Vec<crate::agent::TodoItem> = Vec::new();
    // Per-row markdown render cache (M6). Only the in-flight streaming row is
    // ever re-rendered in the live viewport, so this stays small; committed
    // rows are rendered once at commit time.
    let mut md_cache = MdCache::new();
    // Dirty flag for the render pass: set by any worker message or terminal
    // event; while clear (and not busy) the loop skips term.draw entirely.
    let mut dirty = true; // first frame must paint
    let mut last_draw = Instant::now();

    rows.push(Row::sys_info(welcome_text(&cfg.model, cfg.tools.len())));
    if resume_rows.is_empty() {
        // Mirror the initial task (already in the agent's history) so the user
        // sees what's running. Do NOT re-send it over in_tx: main has already
        // placed it in the agent's messages, and a second send would duplicate
        // the task and create two adjacent user messages (Anthropic rejects
        // that 400). The worker starts the run itself.
        if let Some(task) = initial_task {
            rows.push(Row::User(task.to_string()));
        }
    } else {
        // Resumed session: the transcript already ends with the final user
        // message (including a --task folded into it by main), so mirroring
        // initial_task here would duplicate it. The rebuilt rows are ordinary
        // rows — the first commit pass flushes them into scrollback.
        rows.push(Row::sys_info(format!(
            "[resumed session — {} transcript rows restored]",
            resume_rows.len()
        )));
        rows.extend(resume_rows);
    }
    // The worker auto-runs when the session ends with a user message (an
    // initial task, or a resumed session stopped mid-turn). Show "running" so
    // the status bar is honest; further submissions queue behind this run.
    if auto_start {
        busy = true;
        run_start = Some(Instant::now());
    }

    loop {
        // Body width the streaming splitter estimates its wraps at —
        // refreshed each iteration so a resize is picked up on the next
        // event. Only needs to be close: a drifted estimate raggeds one
        // committed row cosmetically, text is never lost.
        let split_w = content_width(term.size().map(|s| s.width).unwrap_or(80))
            .saturating_sub(ROLE_BODY_INDENT)
            .max(1);
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
                        run_start = None;
                        streaming = false;
                        sub_streaming_name = None;
                        thinking_stream = None;
                        // TurnStart sets this true; without resetting it here a
                        // worker that dies during the thinking phase (before any
                        // TextDelta clears it) would leave a stale flag.
                        thinking = false;
                        tool_running.clear();
                        dirty = true;
                        rows.push(Row::sys_error(
                            "[worker stopped unexpectedly — type /quit to exit]",
                        ));
                    }
                    // Nothing queued can ever be delivered now: drop the
                    // pending texts and their ghost rows. The MdCache slot is
                    // content-keyed so removal can't alias it, but dropping a
                    // slot that may reference a removed row is eager hygiene
                    // (the watermark stays valid — only uncommitted ghosts
                    // are ever removed).
                    queued.clear();
                    if drop_queued_ghosts(&mut rows) {
                        md_cache.clear();
                    }
                    break;
                }
            };
            dirty = true;
            match msg {
                UiMsg::Event(ev) => match ev {
                    AgentEvent::TurnStart(_) => {
                        streaming = false;
                        thinking_stream = None;
                        activity = "thinking".into();
                        thinking = true;
                        tool_running.clear();
                    }
                    AgentEvent::TextDelta(t) => {
                        // First visible output: the thinking placeholder is
                        // superseded by real content (same below at ToolCall).
                        thinking = false;
                        // The answer starts: the trace row is final.
                        thinking_stream = None;
                        // The PARENT is streaming: no parent tool is pending
                        // anymore. (Sub-agent deltas below don't clear — a
                        // sibling parallel tool may legitimately still run.)
                        tool_running.clear();
                        // Parent speaks: close any open sub-agent bubble AND
                        // keep this delta out of it — streaming=false forces
                        // append_delta to open a fresh row even though the sub
                        // bubble is an Assistant row (the old code merged the
                        // parent's text into the sub-agent's bubble).
                        if sub_streaming_name.take().is_some() {
                            streaming = false;
                        }
                        // Guard the assignment: this fires per streamed delta,
                        // so only touch the string on the thinking→writing
                        // transition instead of allocating every chunk.
                        if activity != "writing" {
                            activity = "writing".into();
                        }
                        append_delta(&mut rows, &mut streaming, &t);
                        // Incremental commit: flush finished paragraphs into
                        // scrollback while the rest keeps streaming.
                        split_streaming_row(&mut rows, split_w);
                    }
                    AgentEvent::ThinkingDelta(t) => {
                        // Reasoning trace: its own dim ✻ block, never merged
                        // into the answer row (the old fold made the trace and
                        // the reply read as one blob).
                        thinking = false;
                        tool_running.clear();
                        // Parent speaks: close any open sub bubble so the
                        // trace doesn't merge into it (same rule as TextDelta).
                        if sub_streaming_name.take().is_some() {
                            streaming = false;
                        }
                        if activity != "thinking" {
                            activity = "thinking".into();
                        }
                        append_thinking_delta(&mut rows, &mut thinking_stream, "", &t);
                        // Long traces flush incrementally too (they are often
                        // LONGER than the answer).
                        split_streaming_row(&mut rows, split_w);
                    }
                    AgentEvent::ToolCall { name, input } => {
                        streaming = false;
                        thinking_stream = None;
                        thinking = false;
                        activity = format!("→ {name}");
                        tool_running.on_call(&name);
                        // Strip control chars once at entry so both the
                        // collapsed summary and the expanded pretty-print
                        // render clean.
                        let input = strip_controls_json(&input);
                        let collapsed = compact(&input);
                        rows.push(Row::ToolCall {
                            name,
                            input,
                            collapsed,
                            expanded: false,
                            started: Instant::now(),
                        });
                    }
                    AgentEvent::ToolResult {
                        name,
                        is_error,
                        content,
                    } => {
                        streaming = false;
                        thinking_stream = None;
                        tool_running.on_result(&name);
                        // Content stays RAW: ANSI colors survive into
                        // ansi_guided_lines at render (which strips non-SGR
                        // sequences itself), so strip_controls is gone from
                        // this path.
                        let elapsed = tool_elapsed_for(&rows, &name);
                        let collapsed = crate::agent::preview(&ansi_plain(&content));
                        let expanded = auto_expand_result(is_error, &content);
                        rows.push(Row::ToolResult {
                            collapsed,
                            name,
                            content,
                            is_error,
                            expanded,
                            elapsed,
                        });
                    }
                    AgentEvent::Done { stop_reason } => {
                        streaming = false;
                        thinking_stream = None;
                        // A normal turn end is the common case — no row. Only
                        // abnormal stops (max_tokens/length/error/unknown) are
                        // worth a notice.
                        if !is_normal_stop(&stop_reason) {
                            rows.push(Row::sys_warn(format!("[done: {stop_reason}]")));
                        }
                    }
                    AgentEvent::Compressed {
                        dropped,
                        kept,
                        trigger,
                        ..
                    } => {
                        streaming = false;
                        thinking_stream = None;
                        let how = match trigger {
                            crate::agent::CompressTrigger::Preflight => "preflight",
                            crate::agent::CompressTrigger::Overflow => "overflow",
                            crate::agent::CompressTrigger::Manual => "manual",
                        };
                        rows.push(Row::sys_info(format!(
                            "[context compressed ({how}): dropped {dropped} messages, {kept} kept]"
                        )));
                    }
                    AgentEvent::Pruned {
                        messages,
                        chars_freed,
                    } => {
                        streaming = false;
                        thinking_stream = None;
                        rows.push(Row::sys_info(format!(
                            "[context pruned: {messages} messages lost stale tool output, {chars_freed} chars freed]"
                        )));
                    }
                    AgentEvent::Warning(w) => {
                        rows.push(Row::sys_warn(format!("[warn] {w}")));
                    }
                    AgentEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cache_creation_input_tokens,
                        cache_read_input_tokens,
                    } => {
                        in_tokens = in_tokens.saturating_add(input_tokens);
                        out_tokens = out_tokens.saturating_add(output_tokens);
                        cache_create = cache_create.saturating_add(cache_creation_input_tokens);
                        cache_read = cache_read.saturating_add(cache_read_input_tokens);
                        // Per-request values, not cumulative: overwrite. The
                        // prompt side is input + both cache buckets (Anthropic
                        // splits cached tokens out of input_tokens; OpenAI
                        // reports the whole prompt in input_tokens and leaves
                        // the buckets at 0). + output covers the response the
                        // next request will carry.
                        ctx_tokens = input_tokens
                            + cache_creation_input_tokens
                            + cache_read_input_tokens
                            + output_tokens;
                    }
                    AgentEvent::TodoUpdate { todos } => {
                        // Update the live plan state — the status bar renders a
                        // `plan done/total` segment from it each frame (see
                        // build_status_line). No transcript row: the full
                        // checklist is already visible in the `todo` tool-result
                        // row, and a per-update sys_info line would just clutter
                        // the scrollback. Full-replacement semantics: each call
                        // carries the entire plan, so a plain assign is correct.
                        plan = todos.clone();
                    }
                    AgentEvent::CodeLog(line) => {
                        // Sandbox console.log → committed transcript row (design
                        // L6: reviewable in scrollback and `/export`-able — a
                        // render-only transient would lose the execution log).
                        // sys_info styling keeps it visually apart from replies;
                        // the payload stays bare — the renderer and /export
                        // draw their own `·` marker (see code_log_row).
                        rows.push(code_log_row(line));
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
                        // Strip control chars from the model-supplied name
                        // once, so every use (bubble prefix, row names,
                        // activity segment) is terminal-safe.
                        let name = strip_controls(&name);
                        match &*event {
                            AgentEvent::TextDelta(t) => {
                                thinking = false;
                                // The child's answer starts: its trace row
                                // (or any sibling's) is final.
                                thinking_stream = None;
                                append_sub_delta(&mut rows, &mut sub_streaming_name, &name, t);
                            }
                            AgentEvent::ThinkingDelta(t) => {
                                // The child's trace: its own dim ✻ block with
                                // the child's name on the head line — never
                                // merged into its text bubble (same rule as
                                // the parent's trace vs the answer).
                                thinking = false;
                                // Close the child's text bubble so the answer
                                // restarts in a fresh bubble after the trace.
                                sub_streaming_name = None;
                                append_thinking_delta(&mut rows, &mut thinking_stream, &name, t);
                            }
                            AgentEvent::ToolCall { name: tn, input } => {
                                sub_streaming_name = None;
                                thinking_stream = None;
                                thinking = false;
                                activity = format!("[{name}] → {tn}");
                                let input = strip_controls_json(input);
                                let collapsed = compact(&input);
                                // Same display name as the row below, so the
                                // placeholder and the committed head agree.
                                let row_name = format!("[{name}] {tn}");
                                tool_running.on_call(&row_name);
                                rows.push(Row::ToolCall {
                                    name: row_name,
                                    input,
                                    collapsed,
                                    expanded: false,
                                    started: Instant::now(),
                                });
                            }
                            AgentEvent::ToolResult {
                                name: tn,
                                is_error,
                                content,
                            } => {
                                sub_streaming_name = None;
                                thinking_stream = None;
                                // The row name mirrors the ToolCall arm's
                                // format, so nearest-back matching finds it.
                                // Content stays raw for the ANSI-aware render.
                                let row_name = format!("[{name}] {tn}");
                                tool_running.on_result(&row_name);
                                let elapsed = tool_elapsed_for(&rows, &row_name);
                                let collapsed = crate::agent::preview(&ansi_plain(content));
                                // Same auto-expand contract as the parent arm.
                                let expanded = auto_expand_result(*is_error, content);
                                rows.push(Row::ToolResult {
                                    collapsed,
                                    name: row_name,
                                    content: content.clone(),
                                    is_error: *is_error,
                                    expanded,
                                    elapsed,
                                });
                            }
                            AgentEvent::Done { .. } => {
                                sub_streaming_name = None;
                                thinking_stream = None;
                            }
                            AgentEvent::Warning(w) => {
                                sub_streaming_name = None;
                                thinking_stream = None;
                                rows.push(Row::sys_warn(format!("[{name}] [warn] {w}")));
                            }
                            _ => {} // TurnStart/Compressed/Usage/SubAgent: noisy or folded
                        }
                    }
                    // non_exhaustive forward-compat: AgentEvent may grow new
                    // variants (0.x SemVer window); they render nothing here
                    // until given a dedicated arm. Compile-adaptation only —
                    // no behavior change for the variants above.
                    _ => {}
                },
                UiMsg::Error(e) => {
                    rows.push(Row::sys_error(format!("[error] {e}")));
                    activity.clear();
                    tool_running.clear();
                    // End every open stream: a leftover `streaming` /
                    // `sub_streaming_name` / `thinking_stream` would let the
                    // NEXT run's first TextDelta merge into a stale row (or,
                    // when the last row is this System row, be silently
                    // dropped).
                    streaming = false;
                    sub_streaming_name = None;
                    thinking_stream = None;
                    // Flush the queue: send the next queued message right
                    // away, keeping busy=true on success so no idle gap opens
                    // (a Ctrl-C in that gap would quit instead of cancelling).
                    let flush = flush_next_queued(&mut queued, &mut rows, &mut md_cache, in_tx);
                    apply_flush(
                        flush,
                        &mut busy,
                        &mut run_start,
                        &mut activity,
                        &mut thinking,
                    );
                }
                UiMsg::RunFinished => {
                    streaming = false;
                    sub_streaming_name = None;
                    thinking_stream = None;
                    activity.clear();
                    tool_running.clear();
                    // Flush the queue (same contract as the Error branch).
                    let flush = flush_next_queued(&mut queued, &mut rows, &mut md_cache, in_tx);
                    apply_flush(
                        flush,
                        &mut busy,
                        &mut run_start,
                        &mut activity,
                        &mut thinking,
                    );
                }
                UiMsg::Context(bd) => {
                    // Read-only report: no busy/streaming state is touched —
                    // the command is refused while running anyway, so this
                    // lands in an idle gap.
                    rows.push(Row::sys_info(render_context_report(
                        &bd,
                        cfg.limits.max_context_tokens,
                    )));
                }
                UiMsg::Diag(level, msg) => {
                    // Kernel diag forwarded by the bridge (see UiMsg::Diag):
                    // reuse the ordinary system-row mechanism — the row rides
                    // the normal commit path into scrollback, respecting the
                    // queued-ghost watermark. The text is self-describing
                    // ("[warn] MCP stdio read stopped: …"), so the level only
                    // picks the color.
                    rows.push(match level {
                        tiny_agent::diag::DiagLevel::Info => Row::sys_info(msg),
                        tiny_agent::diag::DiagLevel::Warn => Row::sys_warn(msg),
                    });
                }
            }
        }

        // Commit finalized turns into the terminal scrollback. A row is
        // "finalized" once it will never mutate again: everything except the
        // in-flight streaming Assistant row (skipped while a stream is open)
        // and queued ghosts (removed on flush, never committed). This is the
        // one place the inline model writes the conversation to scrollback, so
        // the host terminal's native selection/scroll/search cover it.
        let stream_open = streaming || sub_streaming_name.is_some() || thinking_stream.is_some();
        let pending_end = commit_pending_end(&rows, committed, stream_open);
        if committed < pending_end {
            // Same 120-col readability cap as the live viewport
            // (content_width): committing at the FULL terminal width
            // re-wrapped every finalized row the moment it left the live
            // zone — a visible reflow jump on >120-col terminals, where the
            // live view had rendered at 120 and the scrollback commit
            // suddenly went wider (external re-review).
            let width = content_width(term.size().map(|s| s.width).unwrap_or(80));
            commit_rows(
                term,
                &rows[committed..pending_end],
                &mut md_cache,
                width,
                pending_end == rows.len(),
            )?;
            committed = pending_end;
            dirty = true;
        }
        // Index of the in-flight row shown live in the viewport's transcript
        // zone (None when nothing is streaming). Computed independently of the
        // commit watermark above — see inflight_row_index — so a streaming
        // Assistant that opened AFTER a queued ghost still renders.
        let inflight_idx = inflight_row_index(&rows, stream_open);

        // Render the live viewport — only when something changed (worker event
        // drained, key handled, first frame). While a run is in flight, also
        // redraw on a fast tick so the spinner animates and the status-bar
        // elapsed time keeps moving. Committed turns are already in scrollback,
        // so this redraws only the small bottom band, not the whole history.
        let clock_tick = busy && last_draw.elapsed() >= Duration::from_millis(120);
        if dirty || clock_tick {
            dirty = false;
            last_draw = Instant::now();
            // The slash popup tracks the input text: any edit sets dirty, so
            // this refresh rides the same redraw.
            popup.update(&input.text());
            term.draw(|f| {
                let area = f.area(); // the INLINE_HEIGHT-row band at the bottom
                let term_w = area.width;
                // Input box grows with the number of lines typed, up to half
                // the viewport so the zone + status stay visible.
                let max_input = ((area.height as usize) / 2).max(3);
                let input_h = (input.line_count() + 2).min(max_input) as u16;
                let [zone_rect, input_rect, status_rect] = Layout::vertical([
                    Constraint::Min(0),
                    Constraint::Length(input_h),
                    Constraint::Length(1),
                ])
                .areas(area);
                let content_w = content_width(term_w);

                // Transcript zone (the rows above the input box inside the
                // viewport): the slash popup while typing a command, else the
                // thinking placeholder before any output, else the in-flight
                // streaming row, else the running-tool placeholder (a tool
                // execution streams nothing of its own). Each is bottom-aligned
                // so its last line sits flush above the input box.
                // Already-committed turns live in the host terminal's
                // scrollback above this viewport.
                if popup.is_open() {
                    let lines = slash_popup_lines(&popup, zone_rect.height as usize);
                    render_zone_top(f, zone_rect, &lines);
                } else if thinking {
                    let line = Line::from(vec![
                        Span::styled(
                            "● ".to_string(),
                            Style::default().fg(AGENT_FG).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled("…", Style::default().fg(dim_fg())),
                    ]);
                    render_zone_tail(f, zone_rect, &[line]);
                } else if let Some(idx) = inflight_idx {
                    if let Some(row) = rows.get(idx) {
                        let live = if matches!(row, Row::Thinking { .. }) {
                            LiveRow::Think
                        } else {
                            LiveRow::Text
                        };
                        let l = build_history_lines(
                            std::slice::from_ref(row),
                            &mut md_cache,
                            content_w,
                            live,
                            false,
                            false,
                        );
                        let wrapped = wrap_history(&l, content_w);
                        render_zone_tail(f, zone_rect, &wrapped);
                    }
                } else if let Some(line) = tool_running_line(&tool_running, Instant::now()) {
                    render_zone_tail(f, zone_rect, &[line]);
                }

                // Input box. The editing prompt is stable; Enter queues while
                // busy. The border frame tracks the run: busy flips the whole
                // frame to AGENT_FG so the bottom chrome reads as one busy
                // unit, idle falls back to USER_FG.
                let spinner_char = SPINNER[(run_start
                    .map(|t| t.elapsed().as_millis() / 100)
                    .unwrap_or(0) as usize)
                    % SPINNER.len()];
                let (prompt, prompt_style) = (
                    "❯ ".to_string(),
                    Style::default().fg(USER_FG).add_modifier(Modifier::BOLD),
                );
                let body = input.text();
                let lines_view = input.lines_view();
                let (crow, ccol) = input.cursor_pos();
                // The input box height is capped, so a long paste can exceed
                // it. Scroll the box to follow the caret and place the caret
                // within the visible window. The same applies HORIZONTALLY: the
                // Paragraph doesn't wrap, so a line wider than the box is
                // clipped and an unclamped caret would be drawn past the border.
                let inner_rows = (input_h as usize).saturating_sub(2);
                let input_scroll = crow
                    .saturating_sub(inner_rows.saturating_sub(1))
                    .min(u16::MAX as usize) as u16;
                let inner_cols = (input_rect.width as usize).saturating_sub(2).max(1);
                let lead = if crow == 0 { prompt.width() } else { 0 };
                let caret_col = ccol + lead; // display column within the caret's line
                let (hscroll, caret_x) = input_hscroll(caret_col, inner_cols);
                let secs = run_start.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                let act = if activity.is_empty() {
                    "working"
                } else {
                    activity.as_str()
                };
                let input_title = build_input_title(
                    busy,
                    spinner_char,
                    act,
                    secs,
                    queued.len(),
                    (input_rect.width as usize).saturating_sub(2),
                );
                let title_fg = chrome_fg(busy);
                // Ghost placeholder on an empty input box. The caret math below
                // is unaffected: an empty buffer's caret sits at the prompt end,
                // in front of the placeholder text.
                // Build one Line per logical line so a multiline buffer renders
                // across rows (a single Line carrying embedded `\n` collapses:
                // ratatui's grapheme iterator filters control chars including
                // `\n`, so the caret row math below would land on an empty row).
                // The prompt spans only the first line; subsequent lines start
                // at the left border so caret `lead` (computed below as
                // prompt width only when crow==0) stays correct.
                let input_lines: Vec<Line> = if body.is_empty() {
                    vec![Line::from(vec![
                        Span::styled(prompt.clone(), prompt_style),
                        Span::styled(INPUT_PLACEHOLDER, Style::default().fg(dim_fg())),
                    ])]
                } else {
                    lines_view
                        .iter()
                        .enumerate()
                        .map(|(i, ln)| {
                            if i == 0 {
                                Line::from(vec![
                                    Span::styled(prompt.clone(), prompt_style),
                                    Span::raw(ln.clone()),
                                ])
                            } else {
                                Line::from(vec![Span::raw(ln.clone())])
                            }
                        })
                        .collect()
                };
                let input_widget = Paragraph::new(input_lines)
                    .block(
                        WBlock::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(title_fg))
                            .title(Span::styled(input_title, Style::default().fg(title_fg))),
                    )
                    .scroll((input_scroll, hscroll));
                f.render_widget(input_widget, input_rect);
                // Place the caret. Column = border(1) + display width of the text
                // before the caret (+ the prompt, on the first line only), minus
                // the horizontal scroll. Display width keeps it correct for wide
                // chars; hscroll keeps it inside the box on over-wide lines.
                let x = input_rect.x + 1 + caret_x;
                let y = input_rect.y + 1 + (crow as u16).saturating_sub(input_scroll);
                f.set_cursor_position((x, y));

                // Status bar. Detailed run activity lives in the input title;
                // the bar only names the broad state so the two regions do not
                // compete. Idle shows no clock — the run elapsed is the title's
                // job.
                let state = if busy { "running" } else { "idle" }.to_string();
                let cache_seg = if cache_read > 0 || cache_create > 0 {
                    format!(
                        " · cache {}w {}r",
                        fmt_tokens(cache_create),
                        fmt_tokens(cache_read)
                    )
                } else {
                    String::new()
                };
                let stats = format!(
                    " · {} in · {} out{cache_seg}",
                    fmt_tokens(in_tokens),
                    fmt_tokens(out_tokens),
                );
                let ctx = fmt_ctx(ctx_tokens, cfg.limits.max_context_tokens);
                // Plan segment: " · plan done/total", green when every step is
                // completed, yellow while work remains. None when the agent
                // hasn't used `todo` (segment absent, not a null).
                let plan_seg = if plan.is_empty() {
                    None
                } else {
                    let total = plan.len();
                    let done = plan.iter().filter(|t| t.status == "completed").count();
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
                    &cwd_path,
                    status_rect.width as usize,
                );
                f.render_widget(Paragraph::new(status), status_rect);
            })?;
        }

        // Keyboard. Poll longer when idle: a keypress wakes the poll
        // immediately, so the longer timeout costs no input latency but
        // avoids spinning the loop while nothing is happening.
        let poll_ms = if busy { 60 } else { 250 };
        if event::poll(Duration::from_millis(poll_ms))? {
            let ev = event::read()?;
            // Any event may change the view (caret, popup, …).
            dirty = true;
            if let Event::Key(key) = ev {
                // Accept key repeats too: on Windows, crossterm reports held
                // keys as Press once then Repeat — filtering to Press only
                // makes held Backspace/arrows delete/move a single char.
                // Release events are still ignored (they'd double-fire).
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                // The slash popup gets first crack while open: Up/Down move
                // its selection instead of the caret, Tab/Enter complete
                // WITHOUT sending, Esc closes it WITHOUT quitting. Modifier
                // chords and all other keys fall through to the normal
                // handlers untouched.
                if popup.is_open() {
                    match popup_key(&mut popup, key.code, key.modifiers) {
                        Some(PopupAction::Consumed) => continue,
                        Some(PopupAction::Dismiss) => {
                            popup.dismiss(&input.text());
                            continue;
                        }
                        Some(PopupAction::Complete(c)) => {
                            input.set_text(&c);
                            popup.dismiss(&c);
                            continue;
                        }
                        None => {}
                    }
                }
                match (key.code, key.modifiers) {
                    // Esc / Ctrl-C: cancel the in-flight turn and stay in the
                    // TUI. When idle the two now DIVERGE: Esc clears the input
                    // box (it quit before, which was easy to hit by accident
                    // right after a run — quit via /quit or /exit now), while
                    // Ctrl-C quits on a SECOND press within 2s (same accident
                    // profile as Esc had: the busy-cancel muscle memory firing
                    // the instant a run ends). raw mode turns Ctrl-C into a
                    // normal key. The transcript lives in native scrollback,
                    // so quitting just releases the live region — the
                    // conversation stays selectable above it.
                    (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        if busy {
                            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                            // End the current assistant bubble: a System row is
                            // now last, so trailing TextDelta events still in
                            // flight start a fresh Assistant row.
                            streaming = false;
                            sub_streaming_name = None;
                            tool_running.clear();
                            rows.push(Row::sys_warn("[cancelling…]"));
                            // Cancelling stops the whole line: queued messages
                            // are dropped too, and their ghost rows go with them.
                            if !queued.is_empty() {
                                rows.push(Row::sys_warn(format!(
                                    "[dropped {} queued message(s)]",
                                    queued.len()
                                )));
                                queued.clear();
                                if drop_queued_ghosts(&mut rows) {
                                    md_cache.clear();
                                }
                            }
                        } else if matches!(key.code, KeyCode::Char('c')) {
                            // Ctrl-C in idle: first press arms (notice row),
                            // second within the window quits. /quit and /exit
                            // stay single-shot.
                            if ctrl_c_should_quit(ctrl_c_armed, Instant::now()) {
                                let _ = in_tx.send(None);
                                return Ok(queued.len());
                            }
                            ctrl_c_armed = Some(Instant::now());
                            rows.push(Row::sys_info("[press Ctrl-C again to quit]"));
                        } else {
                            // Esc in idle: clear the input box instead of
                            // quitting. Quit via /quit or /exit.
                            input.drain_to_string();
                        }
                    }
                    // Alt/Shift+Enter inserts a newline (portable in raw mode;
                    // every terminal reports KeyModifiers::SHIFT truthfully).
                    (KeyCode::Enter, KeyModifiers::ALT) | (KeyCode::Enter, KeyModifiers::SHIFT) => {
                        input.newline();
                    }
                    // Enter sends (queues while busy). The input stays editable
                    // during a run. Exception first: a trailing `\` on the
                    // last line turns Enter into a newline (bash line
                    // continuation) — the one multiline entry path terminals
                    // that don't report Shift/Alt+Enter modifiers (macOS
                    // Terminal.app) can rely on.
                    (KeyCode::Enter, _) => {
                        if input.lines_view().last().is_some_and(|l| l.ends_with('\\')) {
                            input.end();
                            input.backspace(); // drop the backslash
                            input.newline();
                            continue;
                        }
                        if !input.is_empty() {
                            // Strip control chars once at submission, so what
                            // enters rows / input history / the wire is clean.
                            let trimmed = strip_controls(input.text().trim());
                            if !trimmed.is_empty() {
                                match classify(&trimmed) {
                                    SlashCmd::Message => {
                                        if history.last().map(String::as_str) != Some(&trimmed) {
                                            history.push(trimmed.clone());
                                        }
                                        hist_idx = None;
                                        draft.clear();
                                        input.drain_to_string();
                                        if busy {
                                            // Queue behind the in-flight run. A
                                            // dim Row::Queued ghost marks the
                                            // spot; the flush swaps it for the
                                            // real Row::User only when it sends,
                                            // keeping the transcript in true send
                                            // order. The queue depth shows in the
                                            // input box title.
                                            queued.push_back(trimmed.clone());
                                            rows.push(Row::Queued(trimmed));
                                        } else {
                                            rows.push(Row::User(trimmed.clone()));
                                            // Deliver before flipping busy. If the
                                            // worker is gone (panicked mid-run in a
                                            // debug build, or tearing down) the send
                                            // fails — don't leave the UI stuck in
                                            // "running" waiting for a RunFinished that
                                            // will never arrive.
                                            if in_tx.send(Some(Input::Text(trimmed))).is_err() {
                                                busy = false;
                                                // The message never reached the agent —
                                                // drop it from the transcript so the
                                                // on-screen history matches what the agent
                                                // actually saw.
                                                if matches!(rows.last(), Some(Row::User(_))) {
                                                    rows.pop();
                                                }
                                                rows.push(Row::sys_error(
                                                    "[worker stopped — message not delivered; \
                                                     type /quit to exit]",
                                                ));
                                            } else {
                                                busy = true;
                                                run_start = Some(Instant::now());
                                            }
                                        }
                                    }
                                    SlashCmd::Clear if busy => {
                                        // Clearing mid-run would yank history
                                        // out from under the in-flight agent;
                                        // refuse instead of queueing it.
                                        input.drain_to_string();
                                        rows.push(Row::sys_warn(
                                            "[busy — /clear is disabled while running]",
                                        ));
                                    }
                                    SlashCmd::Clear => {
                                        // Slash command — handled in the UI,
                                        // never sent to the model. Cleared from
                                        // the input box but NOT recorded in input
                                        // history (so Alt+Up recalls messages,
                                        // not commands). Already-committed
                                        // scrollback is left in place (it's
                                        // history the user can still scroll to);
                                        // only the live buffer resets.
                                        input.drain_to_string();
                                        rows.clear();
                                        md_cache.clear();
                                        committed = 0;
                                        in_tokens = 0;
                                        out_tokens = 0;
                                        cache_create = 0;
                                        cache_read = 0;
                                        // Status-bar state follows the
                                        // cleared history: a stale ctx% or
                                        // plan done/total segment would
                                        // otherwise linger until the next
                                        // Usage / TodoUpdate event happens
                                        // to overwrite it (external
                                        // re-review).
                                        ctx_tokens = 0;
                                        plan.clear();
                                        // Send before claiming success. If the worker is gone
                                        // (panicked mid-run in a debug build, or tearing down)
                                        // the send fails and the agent-side history is NOT
                                        // cleared — report that honestly, mirroring the text
                                        // path. The local UI reset above still applies.
                                        let cleared = in_tx.send(Some(Input::Clear)).is_ok();
                                        rows.push(if cleared {
                                            Row::sys_info("[history cleared]")
                                        } else {
                                            Row::sys_error(
                                                "[worker stopped — UI cleared but session \
                                                 history not cleared]",
                                            )
                                        });
                                    }
                                    SlashCmd::Context if busy => {
                                        // Same refusal as /clear: the worker
                                        // doesn't pump in_rx inside a run, so
                                        // queueing would only make the report
                                        // arrive after the run looking frozen;
                                        // an honest "disabled while running"
                                        // is better.
                                        input.drain_to_string();
                                        rows.push(Row::sys_warn(
                                            "[busy — /context is disabled while running]",
                                        ));
                                    }
                                    SlashCmd::Context => {
                                        input.drain_to_string();
                                        if in_tx.send(Some(Input::Context)).is_err() {
                                            rows.push(Row::sys_error(
                                                "[worker stopped — context report unavailable]",
                                            ));
                                        }
                                    }
                                    SlashCmd::Quit => {
                                        // /quit (or /exit): leave the TUI. Works
                                        // mid-run too — the quit path cancels the
                                        // in-flight turn and tears down. Esc in
                                        // idle no longer quits, so this (or
                                        // Ctrl-C) is the intentional way out.
                                        input.drain_to_string();
                                        let _ = in_tx.send(None);
                                        return Ok(queued.len());
                                    }
                                    SlashCmd::Export(arg) => {
                                        // /export dumps the conversation as text:
                                        // with a path → that file; without → the
                                        // clipboard if a tool is available, else
                                        // a default file. (Native selection now
                                        // works too, but /export stays for the
                                        // machine-readable whole-transcript dump.)
                                        input.drain_to_string();
                                        let n = rows.len();
                                        let text = render_transcript(&rows);
                                        let row = match arg {
                                            Some(path) => {
                                                use std::fs::OpenOptions;
                                                use std::io::Write;
                                                // Expand `~/` so /export ~/notes.md lands in the
                                                // home dir. create_new refuses to clobber an
                                                // existing file: the TUI has no undo, so a typo'd
                                                // path (e.g. ~/.zshrc) would otherwise truncate
                                                // real content silently. (The clipboard-less
                                                // fallback below keeps overwriting its own scratch
                                                // file on purpose.)
                                                let dest = expand_tilde(&path);
                                                let res = OpenOptions::new()
                                                    .write(true)
                                                    .create_new(true)
                                                    .open(&dest)
                                                    .and_then(|mut f| f.write_all(text.as_bytes()));
                                                match res {
                                                    Ok(_) => Row::sys_info(format!(
                                                        "[exported {n} rows to {dest}]"
                                                    )),
                                                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                                                        Row::sys_error(format!(
                                                            "[export to {dest} failed: file already \
                                                             exists — /export won't overwrite; remove \
                                                             it or choose another path]"
                                                        ))
                                                    }
                                                    Err(e) => Row::sys_error(format!(
                                                        "[export to {dest} failed: {e}]"
                                                    )),
                                                }
                                            }
                                            None => {
                                                if copy_to_clipboard(&text) {
                                                    Row::sys_info(format!(
                                                        "[copied {n} rows to clipboard]"
                                                    ))
                                                } else {
                                                    let path = "tinyagent-export.md";
                                                    match std::fs::write(path, &text) {
                                                        Ok(_) => Row::sys_info(format!(
                                                            "[no clipboard tool found; \
                                                             exported {n} rows to {path}]"
                                                        )),
                                                        Err(e) => Row::sys_error(format!(
                                                            "[export failed: {e}]"
                                                        )),
                                                    }
                                                }
                                            }
                                        };
                                        rows.push(row);
                                    }
                                    SlashCmd::Out(arg) => {
                                        // Reprint a tool result in full.
                                        // Rows commit at their default state
                                        // and post-commit rows have no cursor
                                        // to toggle, so the in-TUI way to read
                                        // a full output is a fresh expanded
                                        // row pushed through the normal commit
                                        // channel (rendering and guides
                                        // unchanged). Works mid-run like the
                                        // other read-only commands.
                                        input.drain_to_string();
                                        let n = arg.unwrap_or(1);
                                        match nth_last_tool_result(&rows, n) {
                                            Some(r) => {
                                                rows.push(r);
                                            }
                                            None => {
                                                let total = rows
                                                    .iter()
                                                    .filter(|r| matches!(r, Row::ToolResult { .. }))
                                                    .count();
                                                rows.push(Row::sys_warn(format!(
                                                    "[no tool result #{n} — {total} result(s) so far]"
                                                )));
                                            }
                                        }
                                    }
                                    cmd => {
                                        input.drain_to_string();
                                        rows.push(Row::sys_info(slash_reply(&cmd, cfg)));
                                    }
                                }
                            }
                        }
                    }
                    // Line-editing shortcuts. The Alt+Left/Right word motions
                    // must precede the plain Left/Right arms below (match arms
                    // are tried in order); the Ctrl chords likewise precede
                    // the plain-char arm at the bottom.
                    (KeyCode::Left, KeyModifiers::ALT) => input.word_left(),
                    (KeyCode::Right, KeyModifiers::ALT) => input.word_right(),
                    (KeyCode::Char('w'), KeyModifiers::CONTROL) => input.delete_word_back(),
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) => input.kill_to_line_start(),
                    (KeyCode::Char('k'), KeyModifiers::CONTROL) => input.kill_to_line_end(),
                    (KeyCode::Char('a'), KeyModifiers::CONTROL) => input.home(),
                    (KeyCode::Char('e'), KeyModifiers::CONTROL) => input.end(),
                    // Ctrl-D: delete forward at the caret (the backspace
                    // mirror); on an EMPTY idle input it quits instead — the
                    // terminal-tool convention. Empty + busy is a no-op:
                    // nothing to delete, and quitting mid-run stays on /quit
                    // or the double-Ctrl-C confirmation.
                    (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                        if !input.is_empty() {
                            input.delete_forward();
                        } else if !busy {
                            let _ = in_tx.send(None);
                            return Ok(queued.len());
                        }
                    }
                    (KeyCode::Backspace, _) => input.backspace(),
                    (KeyCode::Left, _) => input.move_left(),
                    (KeyCode::Right, _) => input.move_right(),
                    // History recall. Alt+Up/Down ALWAYS recall; plain Up/Down
                    // recall when the buffer is single-line (or a browse is
                    // already active) and move the caret across lines otherwise
                    // — the readline/zsh convention, so the empty-input Up that
                    // every terminal user reaches for just works.
                    (KeyCode::Up, KeyModifiers::ALT) => {
                        history_up(&history, &mut hist_idx, &mut draft, &mut input);
                    }
                    (KeyCode::Down, KeyModifiers::ALT) => {
                        history_down(&history, &mut hist_idx, &draft, &mut input);
                    }
                    (KeyCode::Up, _) => {
                        if up_is_history(input.line_count() == 1, hist_idx.is_some()) {
                            history_up(&history, &mut hist_idx, &mut draft, &mut input);
                        } else {
                            input.move_up();
                        }
                    }
                    (KeyCode::Down, _) => {
                        if hist_idx.is_some() {
                            history_down(&history, &mut hist_idx, &draft, &mut input);
                        } else {
                            input.move_down();
                        }
                    }
                    (KeyCode::Home, _) => input.home(),
                    (KeyCode::End, _) => input.end(),
                    // PageUp/PageDown are intentionally not handled: with mouse
                    // capture off, the terminal owns scrollback paging (and the
                    // wheel). There is no app-managed transcript to page.
                    (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                        input.put_char(c);
                    }
                    _ => {}
                }
            } else if let Event::Paste(s) = ev {
                // Bracketed paste: the whole blob (newlines included) arrives
                // as one event and is inserted as input — not split into many
                // Enter-triggered sends the way a raw paste would be. Control
                // chars (ESC/CSI) are stripped; \n and \t survive.
                input.insert_str(&strip_controls(&s));
            }
            // No mouse handler: with capture off, the terminal owns wheel
            // scroll and click-drag-select over the committed scrollback — both
            // "just work" on the real transcript, which is the whole point of
            // the inline model.
        }
    }
}

/// What happened when the queue was flushed after a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flush {
    /// Nothing was queued — the session goes idle.
    Empty,
    /// The oldest queued message was delivered; the caller marks the new run
    /// busy.
    Sent,
    /// Popped but the worker is gone — the message could not be delivered
    /// (a sys_error row records it); the session goes idle.
    Undeliverable,
}

/// Pop the oldest queued message after a run ended and start the next run
/// with it. Shared by the Error and RunFinished arms, whose flush blocks were
/// verbatim duplicates. Contract: pop_front ↔ remove the oldest ghost row ↔
/// append the real Row::User at the END (the finished run's trailing output
/// may already sit after the ghost, so only an append keeps true send order).
/// Row removal shifts indices, so the MdCache slot is dropped alongside
/// (content-keyed: hygiene, not an anti-aliasing necessity).
pub(super) fn flush_next_queued(
    queued: &mut std::collections::VecDeque<String>,
    rows: &mut Vec<Row>,
    md_cache: &mut MdCache,
    in_tx: &mpsc::Sender<Option<Input>>,
) -> Flush {
    let Some(text) = queued.pop_front() else {
        return Flush::Empty;
    };
    if remove_oldest_queued(rows) {
        md_cache.clear();
    }
    rows.push(Row::User(text.clone()));
    if in_tx.send(Some(Input::Text(text))).is_ok() {
        Flush::Sent
    } else {
        rows.push(Row::sys_error(
            "[worker stopped — queued message not delivered; type /quit to exit]",
        ));
        Flush::Undeliverable
    }
}

/// Apply a `flush_next_queued` outcome to the run state. Shared by the Error
/// and RunFinished arms so the flush contract lives in exactly one place:
/// Sent starts the next run with NO idle gap (a Ctrl-C in that gap would
/// quit instead of cancelling), anything else drops the session to idle.
pub(super) fn apply_flush(
    flush: Flush,
    busy: &mut bool,
    run_start: &mut Option<Instant>,
    activity: &mut String,
    thinking: &mut bool,
) {
    match flush {
        Flush::Sent => {
            *busy = true;
            *run_start = Some(Instant::now());
            *activity = "thinking".into();
            *thinking = true;
        }
        Flush::Undeliverable => {
            *busy = false;
            *run_start = None;
            *thinking = false;
        }
        Flush::Empty => {
            *busy = false;
            *run_start = None;
            *thinking = false;
        }
    }
}
