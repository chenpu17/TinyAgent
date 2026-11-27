//! Session wiring: `run` (the entry point main.rs calls), the worker thread,
//! the event/input channels, the terminal guards, and `ui_loop` — the
//! event-handling session loop that ties the other modules together.

use super::input::{history_down, history_up, up_is_history, InputBuffer};
use super::markdown::{ansi_plain, MdCache};
use super::render::{
    build_input_title, build_status_line, chrome_fg, commit_rows, composer_footer, composer_scroll,
    content_width, copy_to_clipboard, empty_state_lines, failed_title, layout_composer,
    queue_preview_line, render_zone_tail, render_zone_top, status_stats, strip_controls,
    strip_controls_json, truncate_display, welcome_text, wrap_history,
};
use super::slash::{
    classify, popup_key, render_context_report, slash_popup_lines_for, slash_reply, PopupAction,
    SlashCmd, SlashPopup,
};
use super::theme::{
    composer_bg, dim_fg, fmt_ctx, set_theme_from_env, surface_bg, AGENT_FG, TOOL_CALL_FG,
    TOOL_ERR_FG, USER_FG,
};
use super::transcript::{
    append_delta, append_sub_delta, append_thinking_delta, auto_expand_result, build_history_lines,
    code_log_row, commit_pending_end, compact, inflight_row_index, latest_detail,
    nth_last_thinking, nth_last_tool_call, nth_last_tool_result, render_transcript,
    split_streaming_row, tool_elapsed_for, tool_running_line, LiveRow, Row, SysLevel, ToolRunning,
    AGENT_MARK, ROLE_BODY_INDENT, TOOL_HEAD_MARK,
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
/// Leading space keeps the terminal's block caret off the first glyph of the
/// placeholder — the empty composer reads as focused, not as a selected "D".
const INPUT_PLACEHOLDER: &str = " Describe a task…  ·  / for commands";

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

/// Display name for a tool call/result. Tool names are model-controlled
/// (execute_tool_call emits whatever the model wrote), so control characters
/// are stripped once here and every consumer — the row name, the activity
/// segment, the `tool_running` key, `tool_elapsed_for`'s match — sees the
/// same sanitized string. Sanitizing only one side would shift the name's
/// display width and break the nearest-back call/result matching.
pub(super) fn tool_display_name(name: &str) -> String {
    strip_controls(name)
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
    /// `/compact [focus]` finished — the completion twin of `RunFinished`
    /// for a fold (see [`CompactReport`]). The fold's own events (the
    /// `Compressed { trigger: Manual }` audit row, a session-save warning,
    /// …) streamed back as `Event`s ahead of this one.
    CompactDone(CompactReport),
}

/// What one `/compact` came to, for the UI loop's completion arm. Built on
/// the worker by [`compact_report`]; `compact_step` delivers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CompactReport {
    /// A summary replaced part of the history. False for "nothing to fold",
    /// an aborted request, a failed one, and the degenerate empty-summary
    /// pass (whose own `Warning` event is the row).
    pub(super) folded: bool,
    /// The summary request was cut short by the cancel flag — the Esc /
    /// Ctrl-C the user pressed while it was in flight. The UI answers with
    /// its one `[cancelled]` row; nothing else is reported for it.
    pub(super) aborted: bool,
    /// A transcript row to add: why nothing folded, or why the request
    /// failed. `None` on a clean fold (its `Compressed` event is the row)
    /// and on an aborted one.
    pub(super) note: Option<(tiny_agent::diag::DiagLevel, String)>,
    /// What the summary request billed — on success AND on failure. The
    /// kernel hands a manual compact's cost to the host (`compact` never
    /// folds it into the run totals, which reset per run), so the status
    /// bar's counters must take it from here or the fold would read as free.
    pub(super) usage: Option<tiny_agent::Usage>,
}

/// Control or text input from the UI to the worker thread. `None` on the
/// channel means "quit" (Esc / idle Ctrl-C); `Some(Text)` is a user message;
/// `Some(Clear)` is the `/clear` command, which the worker applies to the
/// agent's history without running a turn; `Some(Context)` asks for the
/// breakdown snapshot (read-only, no turn); `Some(Compact)` is
/// `/compact [focus]` — the fold runs on the worker, which reports back
/// through [`UiMsg::CompactDone`].
pub(super) enum Input {
    Text(String),
    Clear,
    Context,
    Compact(Option<String>),
}

/// On drop, latches the worker-exited flag and signals worker completion so
/// the quit path can wait a bounded time for the worker to exit (and let a
/// cancel-aware tool kill its child) instead of detaching it mid-tool and
/// orphaning a spawned shell process. The flag is how the UI loop learns of
/// a worker that died mid-run: the diag bridge (installed in `run` before
/// the worker starts) holds a sender clone for the whole session, so the
/// event channel itself never reports Disconnected.
struct DoneSignal(
    mpsc::Sender<()>,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
);

impl Drop for DoneSignal {
    fn drop(&mut self) {
        self.1.store(true, std::sync::atomic::Ordering::Release);
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

fn restore_terminal() {
    let _ = stdout().execute(DisableBracketedPaste);
    let _ = disable_raw_mode();
    tiny_agent::diag::clear_diag_sink();
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// crossterm 0.29's Unix event source loops inside `poll()` when the tty
/// returns EOF: a zero-length read does not break its read loop, and non-
/// WouldBlock errors are ignored. The UI thread therefore never gets a chance
/// to observe an error after the pty master / controlling terminal disappears.
/// Probe the same tty independently so that case still has a teardown path.
#[cfg(unix)]
pub(super) fn terminal_fd_disconnected(
    fd: std::os::fd::RawFd,
    wait: Duration,
) -> std::io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = wait.as_millis().min(i32::MAX as u128) as i32;
    let rc = unsafe { libc::poll(&mut pfd, 1, millis) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0)
}

/// Matches crossterm's tty choice: stdin when it is a terminal, else `/dev/tty`.
/// The owned File keeps a separately opened descriptor alive for the monitor.
#[cfg(unix)]
struct TerminalInput {
    fd: std::os::fd::RawFd,
    _owned: Option<std::fs::File>,
}

#[cfg(unix)]
impl TerminalInput {
    fn new() -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;
        if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
            return Ok(Self {
                fd: libc::STDIN_FILENO,
                _owned: None,
            });
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")?;
        Ok(Self {
            fd: file.as_raw_fd(),
            _owned: Some(file),
        })
    }
}

/// Last-resort teardown for a terminal that no longer exists. The main thread
/// may be trapped in crossterm's internal EOF loop, so this monitor cannot just
/// send it a message. Cancel + shutdown + `None` give the worker the same stop
/// signals as a normal quit; after the same bounded wait, restore what can be
/// restored and exit the CLI rather than leaving a detached 100%-CPU process.
#[cfg(unix)]
fn spawn_terminal_disconnect_monitor(
    in_tx: mpsc::Sender<Option<Input>>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker_exited: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::io::Result<()> {
    use std::sync::atomic::Ordering;
    let terminal = TerminalInput::new()?;
    std::thread::Builder::new()
        .name("tui-tty-watch".into())
        .spawn(move || {
            while !shutdown.load(Ordering::Acquire) {
                match terminal_fd_disconnected(terminal.fd, Duration::from_millis(100)) {
                    Ok(false) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    // An unexpected probe failure must not kill a healthy TUI.
                    Err(_) => break,
                    Ok(true) => {}
                }
                // Normal teardown may have won while the probe was blocked.
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
                cancel.store(true, Ordering::Release);
                shutdown.store(true, Ordering::Release);
                let _ = in_tx.send(None);
                let deadline = Instant::now() + Duration::from_secs(3);
                while !worker_exited.load(Ordering::Acquire) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                restore_terminal();
                std::process::exit(0);
            }
        })?;
    Ok(())
}

#[cfg(not(unix))]
fn spawn_terminal_disconnect_monitor(
    _in_tx: mpsc::Sender<Option<Input>>,
    _cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _worker_exited: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::io::Result<()> {
    Ok(())
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

/// The worker side of `/compact [focus]`: fold on the worker (it owns the
/// agent), stream the fold's own events to the UI exactly like a turn's,
/// then report the outcome as ONE [`UiMsg::CompactDone`]. Split out of the
/// worker loop so the translation is testable without a terminal. Returns
/// false once the UI is gone (the report could not be delivered).
pub(super) fn compact_step(
    agent: &mut Agent,
    focus: Option<&str>,
    out_tx: &mpsc::Sender<UiMsg>,
) -> bool {
    let res = agent.compact_with(
        &mut |ev| {
            let _ = out_tx.send(UiMsg::Event(ev));
        },
        focus,
    );
    // Read AFTER the call: the flag an Esc raised during the summary
    // request is what aborted it (the kernel clears a stale one at entry).
    let report = compact_report(res, agent.cancelled(), agent.limits().keep_recent_messages);
    out_tx.send(UiMsg::CompactDone(report)).is_ok()
}

/// Translate one finished `compact_with` into what the UI loop needs. Pure —
/// every outcome is unit-tested without an agent or a channel.
///
/// Cost is honest on every path: the kernel hands a manual compact's bill to
/// the host (it never folds it into the run totals, which reset per run), so
/// `usage` carries the summary's tokens on success and whatever it burned
/// before failing on error.
pub(super) fn compact_report(
    res: anyhow::Result<Option<tiny_agent::CompressOutcome>>,
    cancelled: bool,
    keep_recent_messages: usize,
) -> CompactReport {
    match res {
        Ok(Some(o)) => CompactReport {
            // `dropped == 0` is the degenerate pass — the summary came back
            // empty; the kernel's Warning event already said so.
            folded: o.dropped > 0,
            aborted: false,
            note: None,
            usage: o.usage,
        },
        Ok(None) => CompactReport {
            folded: false,
            aborted: false,
            note: Some((
                tiny_agent::diag::DiagLevel::Info,
                format!(
                    "[compact] nothing to fold — the history is too short to keep its last \
                     {keep_recent_messages} message(s) AND summarize what precedes them]"
                ),
            )),
            usage: None,
        },
        Err(e) => {
            let burned = e
                .downcast_ref::<tiny_agent::CompressError>()
                .and_then(|c| c.burned);
            // `CompressError` both displays as its `source` and exposes it
            // through `source()`, so `{e:#}` on the anyhow wrapper prints the
            // inner chain TWICE ("A: A") — format that chain directly.
            let text = match e.downcast_ref::<tiny_agent::CompressError>() {
                Some(c) => format!("{:#}", c.source),
                None => format!("{e:#}"),
            };
            // An Esc during the request is the user's call, not a failure
            // to report: the UI prints `[cancelled]` itself, and "request
            // cancelled" after it would only be noise.
            CompactReport {
                folded: false,
                aborted: cancelled,
                note: if cancelled {
                    None
                } else {
                    Some((
                        tiny_agent::diag::DiagLevel::Warn,
                        format!("[compact] failed: {text}"),
                    ))
                },
                usage: burned,
            }
        }
    }
}

/// Status-bar bookkeeping for one finished `/compact`: the summary's bill
/// lands in the four token totals (saturating, like every accumulator), and
/// a real fold drops the ctx segment — it described a history that no longer
/// exists, and the summary's own usage is no measurement of the new one (a
/// one-off request over a bare transcript); the segment stays dark until the
/// next direct request re-measures it, the same reset `/clear` applies.
/// Anything that did not fold leaves the value alone. Pure, so the ledger is
/// pinned without a terminal. `totals` is `[in, out, cache create, cache
/// read]`.
pub(super) fn compact_ledger(
    report: &CompactReport,
    totals: [u64; 4],
    ctx_tokens: u64,
) -> ([u64; 4], u64) {
    let [in_tokens, out_tokens, cache_create, cache_read] = totals;
    let (add_in, add_out, add_create, add_read) = match &report.usage {
        Some(u) => (
            u.input_tokens,
            u.output_tokens,
            u.cache_creation_input_tokens,
            u.cache_read_input_tokens,
        ),
        None => (0, 0, 0, 0),
    };
    (
        [
            in_tokens.saturating_add(add_in),
            out_tokens.saturating_add(add_out),
            cache_create.saturating_add(add_create),
            cache_read.saturating_add(add_read),
        ],
        if report.folded { 0 } else { ctx_tokens },
    )
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
    // Latched "worker thread has exited" flag, set by DoneSignal on ANY exit
    // (normal loop end, early return, panic unwind). The event loop reads it
    // because the diag bridge keeps the event channel connected for the whole
    // session — a worker death mid-run is invisible to channel state.
    let worker_exited = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
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
    // Startup note for an unset model window (`Limits::context_window_hint`):
    // a diag row, because the TUI never shows stderr. Queued before the
    // worker's first event, so it is the first thing the transcript shows.
    if let Some(hint) = cfg.limits.context_window_hint() {
        let _ = out_tx.send(UiMsg::Diag(tiny_agent::diag::DiagLevel::Warn, hint));
    }
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

    // Install the hangup escape hatch before spawning the worker: monitor
    // creation is fallible, and an early `?` after the worker starts would
    // otherwise bypass the cancellation/join teardown at the end of `run`.
    spawn_terminal_disconnect_monitor(
        in_tx.clone(),
        cancel.clone(),
        shutdown.clone(),
        worker_exited.clone(),
    )?;

    // The live tool table (builtin + MCP + the skill pair + custom), read
    // before the worker takes the agent: the welcome row reports what the
    // model actually sees. The config's own list is not that — `tools: []`
    // plus MCP servers is a table of many, not of zero, and assembly always
    // adds the skill pair.
    let n_tools = agent.tool_names().len();
    // The worker thread owns the agent and drives run() per user input.
    let worker_cancel = cancel.clone();
    let worker_shutdown = shutdown.clone();
    let worker_done_flag = worker_exited.clone();
    let worker = std::thread::spawn(move || {
        // Signal completion on ANY exit (normal loop end or early return) via
        // Drop, so the quit path can join within a bounded wait and the UI
        // loop can detect a mid-run death (see `worker_exited`).
        let _done = DoneSignal(done_tx, worker_done_flag);
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
                Input::Compact(focus) => {
                    // `/compact [focus]`: the worker owns the agent, so the
                    // fold runs here; its events (the Compressed audit row,
                    // a session-save warning, …) stream back like a turn's.
                    compact_step(&mut agent, focus.as_deref(), &out_tx);
                }
            }
        }
    });

    let result = ui_loop(
        &mut term,
        cfg,
        &in_tx,
        &out_rx,
        &worker_exited,
        &cancel,
        initial_task.as_deref(),
        auto_start,
        n_tools,
        resume_rows,
    );

    // Stop the worker before restoring the terminal. If terminal restoration
    // errors out below, the worker has already been told to stop (and is
    // detached on scope exit) instead of running unchecked to its next limit.
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    // Session end: fire the SEPARATE teardown flag so the MCP shutdown-reaper
    // kills a wedged server child and unblocks the worker's bounded join below.
    // (cancel alone won't: the worker may be stuck inside a 120s MCP call, not
    // at a turn boundary where cancel is checked.) The disconnect monitor uses
    // the same pair on terminal loss; a per-turn Esc never reaches either path.
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

/// UI run lifecycle. Failed is idle for editing/submission, but its paused
/// queue cannot advance until `/resume`; Cancelling keeps busy-key semantics
/// while making repeated Esc/Ctrl-C idempotent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RunPhase {
    Idle,
    Running,
    Cancelling,
    Failed,
}

impl RunPhase {
    fn busy(self) -> bool {
        matches!(self, RunPhase::Running | RunPhase::Cancelling)
    }

    fn label(self) -> &'static str {
        match self {
            RunPhase::Idle => "idle",
            RunPhase::Running => "running",
            RunPhase::Cancelling => "cancelling",
            RunPhase::Failed => "failed",
        }
    }
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
    worker_exited: &std::sync::atomic::AtomicBool,
    cancel: &std::sync::atomic::AtomicBool,
    initial_task: Option<&str>,
    auto_start: bool,
    n_tools: usize,
    resume_rows: Vec<Row>,
) -> Result<usize> {
    // The full conversation, in append order. Finalized prefixes are committed
    // into the terminal scrollback via insert_before and never re-rendered; the
    // live viewport only ever shows the in-flight tail, so memory grows with
    // the session but per-frame work does not.
    let mut rows: Vec<Row> = Vec::new();
    // Watermark: rows[0..committed] have already been flushed to scrollback and
    // are permanent. Only the open streaming tail is withheld; pending input
    // lives in a separate FIFO and cannot invalidate this index.
    let mut committed: usize = 0;
    let mut input = InputBuffer::new();
    let mut phase = RunPhase::Idle;
    // Failed runs hold the FIFO until an explicit /resume. This survives a
    // separately submitted new prompt, which must not silently release it.
    let mut queue_paused = false;
    // Messages typed while a run is in flight, oldest first. This FIFO is the
    // ONLY queue truth; its preview is render-only and never enters `rows`.
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
    // viewport renders a `◆ …` line from it; nothing is committed for it.
    let mut thinking = false;
    // Transient in-flight tool tracker (render-only, like `thinking`): the
    // ToolCall row commits immediately, so this placeholder is what keeps the
    // live zone showing WHICH tool is running and for how long.
    let mut tool_running = ToolRunning::new();
    // Latest ToolInputDelta, render-only: argument fragments never enter rows.
    let mut tool_building: Option<(String, Instant)> = None;
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
    // summed) from each direct-request Usage event (tool-folded reports —
    // a tool's bill, not a measurement — leave it alone; see ctx_after_usage):
    // prompt tokens (incl. cached parts) plus the response just appended to
    // the history. Drives the status bar's `ctx` segment.
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

    rows.push(Row::sys_info(welcome_text(&cfg.model, n_tools)));
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
        phase = RunPhase::Running;
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
                Err(mpsc::TryRecvError::Empty) => {
                    // The diag bridge (installed in `run` for the whole
                    // session) holds a sender clone, so the channel reports
                    // Empty — never Disconnected — even after the worker's
                    // own sender died with its thread. A worker gone mid-run
                    // (a panic in a debug build; release aborts the process)
                    // is therefore detected by the flag DoneSignal set on
                    // drop, not by channel state.
                    if phase.busy()
                        && worker_exited.load(std::sync::atomic::Ordering::Acquire)
                        && reset_after_worker_exit(
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
                        )
                    {
                        dirty = true;
                    }
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Same reset as the worker-exited arm above. Defense in
                    // depth: unreachable while the diag bridge holds its
                    // sender clone, but kept so a future teardown that drops
                    // the bridge mid-session still resets the UI instead of
                    // hanging in "running".
                    if reset_after_worker_exit(
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
                    ) {
                        dirty = true;
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
                        tool_building = None;
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
                        tool_building = None;
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
                        tool_building = None;
                        // Parent speaks: close any open sub bubble so the
                        // trace doesn't merge into it (same rule as TextDelta).
                        if sub_streaming_name.take().is_some() {
                            streaming = false;
                        }
                        if activity != "thinking" {
                            activity = "thinking".into();
                        }
                        append_thinking_delta(&mut rows, &mut thinking_stream, "", &t);
                        // Thinking commits as one compact/folded row at close;
                        // only Assistant text uses incremental row splitting.
                    }
                    AgentEvent::ToolInputDelta { name, .. } => {
                        let name = tool_display_name(&name);
                        thinking = false;
                        // Argument construction follows any text/reasoning from
                        // this assistant turn. Close those live rows so the new
                        // render-only tool-building state is actually visible.
                        streaming = false;
                        sub_streaming_name = None;
                        thinking_stream = None;
                        activity = format!("→ {name} · receiving args");
                        if tool_building.as_ref().is_none_or(|(n, _)| n != &name) {
                            tool_building = Some((name, Instant::now()));
                        }
                    }
                    AgentEvent::ToolCall { name, input } => {
                        tool_building = None;
                        streaming = false;
                        thinking_stream = None;
                        thinking = false;
                        // Sanitize once, before every consumer: the row, the
                        // activity segment, and the tool tracker key all see
                        // the same string (see tool_display_name).
                        let name = tool_display_name(&name);
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
                        // Same sanitized key as the ToolCall arm above, so
                        // the duration match and the tracker removal line up.
                        let name = tool_display_name(&name);
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
                    AgentEvent::NoteInjected { text, deferred, .. } => {
                        // Supervisor note (R1): a control signal injected by
                        // an external observer — warn-level visibility so it
                        // stands out; /export carries it like any row. The
                        // deferred tag says it missed every delivery point
                        // this run and rides history into the next one.
                        if deferred {
                            rows.push(Row::sys_warn(format!(
                                "[supervisor note, deferred to next run] {text}"
                            )));
                        } else {
                            rows.push(Row::sys_warn(format!("[supervisor note] {text}")));
                        }
                    }
                    AgentEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cache_creation_input_tokens,
                        cache_read_input_tokens,
                        tool_folded,
                        // A split of output_tokens — the status bar keeps its
                        // four counters (the TUI stays minimal).
                        reasoning_tokens: _,
                    } => {
                        in_tokens = in_tokens.saturating_add(input_tokens);
                        out_tokens = out_tokens.saturating_add(output_tokens);
                        cache_create = cache_create.saturating_add(cache_creation_input_tokens);
                        cache_read = cache_read.saturating_add(cache_read_input_tokens);
                        // Only a DIRECT provider request re-measures the
                        // context — see ctx_after_usage: a tool-folded report
                        // (a `task` child's multi-turn sum, a run_code inner
                        // llm, a host tool's bill) must not SET the ctx
                        // segment (it would show a bogus, possibly >100%
                        // number until the next real request). Totals above
                        // still count it — honest cost, skipped only here.
                        ctx_tokens = ctx_after_usage(
                            ctx_tokens,
                            tool_folded,
                            input_tokens,
                            cache_creation_input_tokens,
                            cache_read_input_tokens,
                            output_tokens,
                        );
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
                        // Sanitize the model-supplied name once, so every use
                        // (bubble prefix, row names, activity segment) is
                        // terminal-safe.
                        let name = tool_display_name(&name);
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
                                tool_building = None;
                                sub_streaming_name = None;
                                thinking_stream = None;
                                thinking = false;
                                // Same sanitized display name as the row
                                // below, so the placeholder and the committed
                                // head agree (and their tracker keys match).
                                let tn = tool_display_name(tn);
                                activity = format!("[{name}] → {tn}");
                                let input = strip_controls_json(input);
                                let collapsed = compact(&input);
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
                                // format (with the same sanitized tn), so
                                // nearest-back matching finds it. Content
                                // stays raw for the ANSI-aware render.
                                let tn = tool_display_name(tn);
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
                            _ => {} // the child's internal bookkeeping (TurnStart/Usage/Compressed/Pruned/MaxTokensSalvage/…): folded at the agent level
                        }
                    }
                    AgentEvent::MaxTokensSalvage => {
                        // The wrap-up turn after a max_tokens cutoff is billed
                        // and hook-visible like any turn, but its trigger is
                        // otherwise invisible here (harness writes stderr /
                        // NDJSON `{"type":"salvage"}`) — warn level matches
                        // the `[done: max_tokens]` notice above.
                        rows.push(Row::sys_warn(
                            "[max_tokens hit — one wrap-up turn appended]",
                        ));
                    }
                    AgentEvent::TurnArbitration {
                        continued, note, ..
                    } => {
                        // 轮末仲裁（FR-1）：仅在宿主 hook 给出非默认决策时
                        // 出现。continued 的投递可见性由随后的 NoteInjected
                        // 行承担，这里标决策点本身；capped 的 note 不会投递
                        // （只活在事件里），所以把它带上——否则这段意图在
                        // 会话里就无迹可寻了。warn 级与 NoteInjected 对齐。
                        // 流式卫生（round-3 评审 R1-10）：与 Warning 臂同款
                        // 重置——仲裁行之后到下轮 TurnStart 之间，残留的
                        // streaming 状态会把系统行错当 in-flight 流式行。
                        sub_streaming_name = None;
                        thinking_stream = None;
                        if continued {
                            rows.push(Row::sys_warn(
                                "[arbitration: completion rejected — one more turn]",
                            ));
                        } else {
                            rows.push(Row::sys_warn(format!(
                                "[arbitration capped — note not delivered] {note}"
                            )));
                        }
                    }
                    // ToolInputDelta is streaming plumbing: the ToolCall row
                    // and the live tool placeholder own its presentation, and
                    // a row per fragment would flood the transcript.
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
                    tool_building = None;
                    // End every open stream: a leftover streaming flag would
                    // let the next run merge into a stale row.
                    streaming = false;
                    sub_streaming_name = None;
                    thinking_stream = None;
                    thinking = false;
                    run_start = None;
                    phase = RunPhase::Failed;
                    queue_paused = pause_for_held(true, queued.len());
                    if !queued.is_empty() {
                        rows.push(Row::sys_warn(format!(
                            "[queue paused — {} message(s) held; /resume continues, /unqueue edits]",
                            queued.len()
                        )));
                    }
                }
                msg @ (UiMsg::RunFinished | UiMsg::CompactDone(_)) => {
                    // A finished `/compact` takes the run's completion path
                    // (busy state cleared, the queue flushed) after its own
                    // bookkeeping: an optional row, the summary's bill into
                    // the status-bar totals, and the ctx segment dropped by a
                    // real fold (see `compact_ledger`).
                    let compact = match &msg {
                        UiMsg::CompactDone(report) => Some(report),
                        _ => None,
                    };
                    if let Some(report) = compact {
                        if let Some((level, text)) = &report.note {
                            rows.push(match level {
                                tiny_agent::diag::DiagLevel::Info => Row::sys_info(text.clone()),
                                tiny_agent::diag::DiagLevel::Warn => Row::sys_warn(text.clone()),
                            });
                        }
                        let (totals, ctx) = compact_ledger(
                            report,
                            [in_tokens, out_tokens, cache_create, cache_read],
                            ctx_tokens,
                        );
                        [in_tokens, out_tokens, cache_create, cache_read] = totals;
                        ctx_tokens = ctx;
                    }
                    streaming = false;
                    sub_streaming_name = None;
                    thinking_stream = None;
                    activity.clear();
                    tool_running.clear();
                    tool_building = None;
                    // A run was cancelled iff Esc was pressed; a compact
                    // only if the cancel really aborted it (`aborted`).
                    let aborted = match compact {
                        Some(report) => report.aborted,
                        None => phase == RunPhase::Cancelling,
                    };
                    if aborted {
                        // Exactly one terminal notice; the initial keypress
                        // already contributed the waiting-state row.
                        rows.push(Row::sys_warn("[cancelled]"));
                        phase = RunPhase::Idle;
                        run_start = None;
                        thinking = false;
                    } else if compact.is_some() && phase == RunPhase::Cancelling {
                        // Only reachable for a compact: an Esc raced a fold
                        // that landed anyway (or a request that concluded on
                        // its own). The outcome's row is already up — report
                        // what happened, not a cancel that did nothing. The
                        // queue is empty here: the Esc that entered Cancelling
                        // dropped it.
                        phase = RunPhase::Idle;
                        run_start = None;
                        thinking = false;
                    } else if queue_paused {
                        // A separate prompt may have completed while an older
                        // failed queue remains explicitly held.
                        phase = RunPhase::Idle;
                        run_start = None;
                        thinking = false;
                    } else {
                        let flush = flush_next_queued(&mut queued, &mut rows, in_tx);
                        apply_flush(
                            flush,
                            &mut phase,
                            &mut run_start,
                            &mut activity,
                            &mut thinking,
                        );
                    }
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
                    // the normal commit path into scrollback. The text is
                    // self-describing ("[warn] MCP stdio read stopped: …"), so
                    // the level only picks the color.
                    rows.push(match level {
                        tiny_agent::diag::DiagLevel::Info => Row::sys_info(msg),
                        tiny_agent::diag::DiagLevel::Warn => Row::sys_warn(msg),
                    });
                }
            }
        }

        // Commit finalized turns into the terminal scrollback. A row is
        // "finalized" once it will never mutate again: everything except the
        // in-flight streaming Assistant/Thinking row (skipped while its stream
        // is open). Pending input is outside `rows` and never blocks a commit.
        // This is the one place the inline model writes the conversation to
        // scrollback, so
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
        // commit watermark above — see inflight_row_index — so a later streaming
        // Assistant always renders even after earlier rows have finalized.
        let inflight_idx = inflight_row_index(&rows, stream_open);

        // Render the live viewport — only when something changed (worker event
        // drained, key handled, first frame). While a run is in flight, also
        // redraw on a fast tick so the spinner animates and the status-bar
        // elapsed time keeps moving. Committed turns are already in scrollback,
        // so this redraws only the small bottom band, not the whole history.
        let clock_tick = phase.busy() && last_draw.elapsed() >= Duration::from_millis(120);
        if dirty || clock_tick {
            dirty = false;
            last_draw = Instant::now();
            // The slash popup tracks the input text: any edit sets dirty, so
            // this refresh rides the same redraw.
            popup.update_for(&input.text(), phase.busy());
            term.draw(|f| {
                let area = f.area(); // the INLINE_HEIGHT-row band at the bottom
                let busy = phase.busy();
                let term_w = area.width;
                let (prompt, prompt_style) = (
                    "❯ ".to_string(),
                    Style::default().fg(USER_FG).add_modifier(Modifier::BOLD),
                );
                let chrome_w = content_width(term_w) as u16;
                let inner_cols = (chrome_w as usize).saturating_sub(2).max(1);
                let composer = layout_composer(&input, inner_cols, &prompt, prompt_style);
                // Soft-wrapped rows, not logical newlines, determine height.
                // The fixed inline viewport keeps at least a small live zone.
                let max_input = ((area.height as usize) / 2).max(3);
                let input_h = (composer.lines.len() + 2).clamp(3, max_input) as u16;
                let queue_line = queue_preview_line(&queued, queue_paused, chrome_w as usize);
                let queue_h = if queue_line.is_some() { 1 } else { 0 };
                let [zone_rect, mut queue_rect, mut input_rect, mut status_rect] =
                    Layout::vertical([
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
                if popup.is_open() {
                    let lines = slash_popup_lines_for(&popup, zone_rect.height as usize, content_w);
                    render_zone_top(f, zone_rect, &lines);
                } else if phase == RunPhase::Idle && rows.len() <= 1 && input.is_empty() {
                    let lines = empty_state_lines(content_w);
                    render_zone_top(f, zone_rect, &lines);
                } else if thinking {
                    let line = Line::from(vec![
                        Span::styled(
                            AGENT_MARK.to_string(),
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
                } else if let Some((name, started)) = tool_building.as_ref().filter(|_| busy) {
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
                } else if let Some(line) = tool_running_line(&tool_running, Instant::now()) {
                    render_zone_tail(f, zone_rect, &[line]);
                }
                if let Some(line) = queue_line {
                    f.render_widget(Paragraph::new(line), queue_rect);
                }

                // Input box. The editing prompt is stable; Enter queues while
                // busy. The border frame tracks the run: busy flips the whole
                // frame to AGENT_FG so the bottom chrome reads as one busy
                // unit, idle falls back to USER_FG.
                let spinner_char = SPINNER[(run_start
                    .map(|t| t.elapsed().as_millis() / 100)
                    .unwrap_or(0) as usize)
                    % SPINNER.len()];
                let body = input.text();
                // Vertical scrolling follows the caret through soft-wrapped
                // rows. No horizontal scroll: long prompts remain reviewable.
                let inner_rows = (input_h as usize).saturating_sub(2);
                let input_scroll = composer_scroll(composer.caret_row, inner_rows);
                let secs = run_start.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                let act = if activity.is_empty() {
                    "working"
                } else {
                    activity.as_str()
                };
                let title_w = (input_rect.width as usize).saturating_sub(2);
                let input_title = match phase {
                    RunPhase::Cancelling => {
                        truncate_display(" ⠹ cancelling… · waiting for stop ", title_w)
                    }
                    RunPhase::Failed => failed_title(queued.len(), title_w),
                    _ => build_input_title(busy, spinner_char, act, secs, queued.len(), title_w),
                };
                let title_fg = match phase {
                    RunPhase::Cancelling => TOOL_CALL_FG,
                    RunPhase::Failed => TOOL_ERR_FG,
                    _ => chrome_fg(busy),
                };
                let mut input_lines = composer.lines;
                if body.is_empty() {
                    if let Some(first) = input_lines.first_mut() {
                        first.spans.push(Span::styled(
                            truncate_display(
                                INPUT_PLACEHOLDER,
                                inner_cols.saturating_sub(prompt.width()),
                            ),
                            Style::default().fg(dim_fg()),
                        ));
                    }
                }
                let border_fg = match phase {
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
                let x = input_rect.x
                    + 1
                    + (composer.caret_col.min(inner_cols.saturating_sub(1)) as u16);
                let y = input_rect.y + 1 + (composer.caret_row as u16).saturating_sub(input_scroll);
                f.set_cursor_position((x, y));

                // Status bar. Detailed run activity lives in the input title;
                // the bar only names the broad state so the two regions do not
                // compete. Idle shows no clock — the run elapsed is the title's
                // job.
                let state = phase.label().to_string();
                // A brand-new session has nothing to report: showing
                // "0 in / 0 out" is pure noise. The segment appears with the
                // first billed turn (cache-only counters count as activity).
                let stats = status_stats(in_tokens, out_tokens, cache_create, cache_read);
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
                f.render_widget(
                    Paragraph::new(status).style(Style::default().bg(surface_bg())),
                    status_rect,
                );
            })?;
        }

        // Keyboard. Poll longer when idle: a keypress wakes the poll
        // immediately, so the longer timeout costs no input latency but
        // avoids spinning the loop while nothing is happening.
        let poll_ms = if phase.busy() { 60 } else { 250 };
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
                        if phase.busy() {
                            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                            if phase == RunPhase::Running {
                                phase = RunPhase::Cancelling;
                                activity = "cancelling".into();
                                // End the current assistant bubble: trailing
                                // deltas start a fresh row after this notice.
                                streaming = false;
                                sub_streaming_name = None;
                                tool_running.clear();
                                tool_building = None;
                                rows.push(Row::sys_warn("[cancelling…]"));
                                if !queued.is_empty() {
                                    rows.push(Row::sys_warn(format!(
                                        "[dropped {} queued message(s)]",
                                        queued.len()
                                    )));
                                    queued.clear();
                                    // The pause is a property of held
                                    // messages: nothing held, nothing paused.
                                    queue_paused = false;
                                }
                            }
                            // Already Cancelling: keep the signal latched but
                            // do not append another row on key repeat.
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
                                let command = classify(&trimmed);
                                match command.clone() {
                                    SlashCmd::Message => {
                                        if history.last().map(String::as_str) != Some(&trimmed) {
                                            history.push(trimmed.clone());
                                        }
                                        hist_idx = None;
                                        draft.clear();
                                        input.drain_to_string();
                                        if phase.busy() {
                                            // Queue behind the in-flight run.
                                            // Pending text stays in the FIFO;
                                            // only its preview is drawn live.
                                            queued.push_back(trimmed.clone());
                                        } else {
                                            rows.push(Row::User(trimmed.clone()));
                                            // Deliver before flipping busy. If the
                                            // worker is gone (panicked mid-run in a
                                            // debug build, or tearing down) the send
                                            // fails — don't leave the UI stuck in
                                            // "running" waiting for a RunFinished that
                                            // will never arrive.
                                            if in_tx.send(Some(Input::Text(trimmed))).is_err() {
                                                phase = RunPhase::Failed;
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
                                                phase = RunPhase::Running;
                                                run_start = Some(Instant::now());
                                                activity = "thinking".into();
                                                thinking = true;
                                            }
                                        }
                                    }
                                    SlashCmd::Clear if phase.busy() => {
                                        // Keep the command in the composer for
                                        // an idle retry; clearing mid-run would
                                        // yank history under the active agent.
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
                                    SlashCmd::Context if phase.busy() => {
                                        // Same refusal as /clear: the worker
                                        // doesn't pump in_rx inside a run, so
                                        // queueing would only make the report
                                        // arrive after the run looking frozen;
                                        // an honest "disabled while running"
                                        // is better.
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
                                    SlashCmd::Compact(_) if phase.busy() => {
                                        // Same refusal as /clear: a fold
                                        // rewrites the history the active
                                        // run is working on. The command
                                        // stays in the composer for an
                                        // idle retry.
                                        rows.push(Row::sys_warn(
                                            "[busy — /compact is disabled while running]",
                                        ));
                                    }
                                    SlashCmd::Compact(focus) => {
                                        input.drain_to_string();
                                        if in_tx.send(Some(Input::Compact(focus))).is_err() {
                                            rows.push(Row::sys_error(
                                                "[worker stopped — compact unavailable]",
                                            ));
                                        } else {
                                            // The summary is a real, billed
                                            // model request — busy like a
                                            // turn, so Esc cancels it and
                                            // typed messages queue behind.
                                            phase = RunPhase::Running;
                                            run_start = Some(Instant::now());
                                            activity = "compacting".into();
                                            thinking = false;
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
                                        if matches!(
                                            row,
                                            Row::System {
                                                level: SysLevel::Error,
                                                ..
                                            }
                                        ) {
                                            input.set_text(&trimmed);
                                        }
                                        rows.push(row);
                                    }
                                    SlashCmd::Queue => {
                                        input.drain_to_string();
                                        if queued.is_empty() {
                                            rows.push(Row::sys_info("[queue empty]"));
                                        } else {
                                            for (i, text) in queued.iter().enumerate() {
                                                rows.push(Row::sys_info(format!(
                                                    "[queued #{} · not sent]\n{text}",
                                                    i + 1
                                                )));
                                            }
                                        }
                                    }
                                    SlashCmd::Unqueue => {
                                        input.drain_to_string();
                                        match queued.pop_back() {
                                            Some(text) => {
                                                input.set_text(&text);
                                                hist_idx = None;
                                                draft.clear();
                                                // Withdrawing the last held
                                                // message lifts the pause.
                                                queue_paused =
                                                    pause_for_held(queue_paused, queued.len());
                                            }
                                            None => rows.push(Row::sys_warn(
                                                "[queue empty — nothing to withdraw]",
                                            )),
                                        }
                                    }
                                    SlashCmd::Resume if phase.busy() => {
                                        input.drain_to_string();
                                        rows.push(Row::sys_warn(
                                            "[busy — /resume continues a paused queue after the run]",
                                        ));
                                    }
                                    SlashCmd::Resume => {
                                        input.drain_to_string();
                                        if !queue_paused && queued.is_empty() {
                                            rows.push(Row::sys_info("[no paused queue]"));
                                        } else {
                                            queue_paused = false;
                                            let flush =
                                                flush_next_queued(&mut queued, &mut rows, in_tx);
                                            apply_flush(
                                                flush,
                                                &mut phase,
                                                &mut run_start,
                                                &mut activity,
                                                &mut thinking,
                                            );
                                        }
                                    }
                                    SlashCmd::Call(arg) => {
                                        input.drain_to_string();
                                        let n = arg.unwrap_or(1);
                                        match nth_last_tool_call(&rows, n) {
                                            Some(r) => rows.push(r),
                                            None => {
                                                let total = rows
                                                    .iter()
                                                    .filter(|r| matches!(r, Row::ToolCall { .. }))
                                                    .count();
                                                rows.push(Row::sys_warn(format!(
                                                    "[no tool call #{n} — {total} call(s) so far]"
                                                )));
                                                input.set_text(&trimmed);
                                            }
                                        }
                                    }
                                    SlashCmd::Thinking(arg) => {
                                        input.drain_to_string();
                                        let n = arg.unwrap_or(1);
                                        match nth_last_thinking(&rows, n) {
                                            Some(r) => rows.push(r),
                                            None => {
                                                let total = rows
                                                    .iter()
                                                    .filter(|r| matches!(r, Row::Thinking { .. }))
                                                    .count();
                                                rows.push(Row::sys_warn(format!(
                                                    "[no thinking block #{n} — {total} block(s) so far]"
                                                )));
                                                input.set_text(&trimmed);
                                            }
                                        }
                                    }
                                    SlashCmd::Out(arg) => {
                                        input.drain_to_string();
                                        let n = arg.unwrap_or(1);
                                        match nth_last_tool_result(&rows, n) {
                                            Some(r) => rows.push(r),
                                            None => {
                                                let total = rows
                                                    .iter()
                                                    .filter(|r| matches!(r, Row::ToolResult { .. }))
                                                    .count();
                                                rows.push(Row::sys_warn(format!(
                                                    "[no tool result #{n} — {total} result(s) so far]"
                                                )));
                                                input.set_text(&trimmed);
                                            }
                                        }
                                    }
                                    cmd @ SlashCmd::Unknown(_) => {
                                        // Correctable input stays in the composer.
                                        rows.push(Row::sys_warn(slash_reply(&cmd, cfg)));
                                    }
                                    cmd => {
                                        input.drain_to_string();
                                        rows.push(Row::sys_info(slash_reply(&cmd, cfg)));
                                    }
                                }
                            }
                        }
                    }
                    // Detail and line-editing shortcuts. Word motions must
                    // precede plain arrows; Ctrl chords precede plain chars.
                    (KeyCode::Char('o'), KeyModifiers::CONTROL) => match latest_detail(&rows) {
                        Some(row) => rows.push(row),
                        None => rows.push(Row::sys_info(
                            "[no tool call, result or thinking detail yet]",
                        )),
                    },
                    (KeyCode::Char('j'), KeyModifiers::CONTROL) => input.newline(),
                    (KeyCode::Left, m)
                        if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
                    {
                        input.word_left()
                    }
                    (KeyCode::Right, m)
                        if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
                    {
                        input.word_right()
                    }
                    (KeyCode::Backspace, KeyModifiers::CONTROL) => input.delete_word_back(),
                    (KeyCode::Delete, _) => input.delete_forward(),
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
                        } else if !phase.busy() {
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

/// Pop the oldest queued message and start its run. The pending FIFO never
/// inserted ghost Rows, so no committed index can move here; the real User row
/// is appended only at the send boundary, preserving true conversation order.
pub(super) fn flush_next_queued(
    queued: &mut std::collections::VecDeque<String>,
    rows: &mut Vec<Row>,
    in_tx: &mpsc::Sender<Option<Input>>,
) -> Flush {
    let Some(text) = queued.pop_front() else {
        return Flush::Empty;
    };
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

/// Re-derive the queue pause from what is actually held, at the two moments
/// the flag follows the queue: after a failed run (`paused` = true) and after
/// `/unqueue` withdrew a message (`paused` = the flag before it). The pause is
/// a property of the held messages — invariant: paused ⇒ non-empty queue. A
/// stale flag on an empty queue would mark messages queued during a later
/// healthy run as "paused", held when it ends without the user ever being told
/// about a pause. A withdrawal never sets the flag: a live (unpaused) queue
/// stays live.
pub(super) fn pause_for_held(paused: bool, held: usize) -> bool {
    paused && held > 0
}

/// Apply a `flush_next_queued` outcome to the run state. Sent starts the next
/// run with NO idle gap (Ctrl-C must cancel, not quit); everything else idles.
pub(super) fn apply_flush(
    flush: Flush,
    phase: &mut RunPhase,
    run_start: &mut Option<Instant>,
    activity: &mut String,
    thinking: &mut bool,
) {
    match flush {
        Flush::Sent => {
            *phase = RunPhase::Running;
            *run_start = Some(Instant::now());
            *activity = "thinking".into();
            *thinking = true;
        }
        Flush::Undeliverable => {
            *phase = RunPhase::Failed;
            *run_start = None;
            *thinking = false;
        }
        Flush::Empty => {
            *phase = RunPhase::Idle;
            *run_start = None;
            *thinking = false;
        }
    }
}

/// Reset the run state after the worker thread died mid-run: drop the UI out
/// of "running" (the RunFinished that would reset it is never coming),
/// surface the death, and drop the queued messages — nothing can deliver
/// them anymore. Shared by the event loop's worker-exited and channel-
/// disconnected arms so the two paths cannot drift. Returns whether anything
/// changed (the caller turns that into a repaint).
#[allow(clippy::too_many_arguments)]
pub(super) fn reset_after_worker_exit(
    phase: &mut RunPhase,
    run_start: &mut Option<Instant>,
    streaming: &mut bool,
    sub_streaming_name: &mut Option<String>,
    thinking_stream: &mut Option<String>,
    thinking: &mut bool,
    activity: &mut String,
    tool_running: &mut ToolRunning,
    queued: &mut std::collections::VecDeque<String>,
    rows: &mut Vec<Row>,
) -> bool {
    let mut changed = false;
    if phase.busy() {
        *phase = RunPhase::Failed;
        *run_start = None;
        *streaming = false;
        *sub_streaming_name = None;
        *thinking_stream = None;
        *thinking = false;
        activity.clear();
        tool_running.clear();
        rows.push(Row::sys_error(
            "[worker stopped unexpectedly — type /quit to exit]",
        ));
        changed = true;
    }
    // No worker can accept these messages; the FIFO contains no transcript
    // ghosts, so dropping it cannot disturb committed row indices.
    if !queued.is_empty() {
        rows.push(Row::sys_warn(format!(
            "[dropped {} queued message(s)]",
            queued.len()
        )));
        changed = true;
    }
    queued.clear();
    changed
}

/// Context occupancy implied by one Usage report: the full prompt (input
/// plus both cache buckets) plus the response just appended to the history.
/// Saturating like every other token accumulator: the four values are
/// provider-reported u64s, so a hostile report must neither panic on the
/// debug overflow check nor wrap into a small number in release.
pub(super) fn ctx_from_usage(
    input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    output_tokens: u64,
) -> u64 {
    input_tokens
        .saturating_add(cache_creation_input_tokens)
        .saturating_add(cache_read_input_tokens)
        .saturating_add(output_tokens)
}

/// The new ctx-segment value after one Usage event. Only a DIRECT provider
/// request (`tool_folded == false`) measures the current history, so it SETs
/// the segment via [`ctx_from_usage`]; a tool-folded report (a `task` child's
/// multi-turn aggregate, the run_code inner llm, a host tool's own bill) is
/// NOT a measurement of this history — a sub-agent's turns sum up here and
/// can exceed the whole window — so it leaves the previous value alone (its
/// tokens still count into the totals; only the ctx segment skips it).
pub(super) fn ctx_after_usage(
    current: u64,
    tool_folded: bool,
    input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    output_tokens: u64,
) -> u64 {
    if tool_folded {
        current
    } else {
        ctx_from_usage(
            input_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            output_tokens,
        )
    }
}
