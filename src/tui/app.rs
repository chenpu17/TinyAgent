//! Session wiring: `run` — the entry point main.rs calls — and the event loop.
//!
//! The loop is four steps per iteration, always in this order: drain the
//! worker channel into the session (`events`), commit finished rows to the
//! terminal scrollback and repaint the live viewport when something changed
//! (`view`), then handle at most one terminal event (`keys` / `commands`). All
//! state lives in the one [`Session`]; this file keeps none of its own.

use super::keys::Effect;
use super::render::content_width;
use super::session::{Session, Startup};
use super::terminal::{install_diag_bridge, spawn_terminal_disconnect_monitor, TerminalGuard};
use super::theme::set_theme_from_env;
use super::transcript::{Row, ROLE_BODY_INDENT};
use super::view;
use super::worker::{worker_main, DoneSignal, Input, UiMsg};
use crate::agent::Agent;
use crate::config::Config;
use anyhow::Result;
use crossterm::event::{self, EnableBracketedPaste, Event};
use crossterm::terminal::enable_raw_mode;
use crossterm::ExecutableCommand;
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::io::stdout;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// Run the TUI session until the user quits (or the terminal goes away).
/// Returns after the worker has been told to stop and the terminal restored.
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
            viewport: Viewport::Inline(view::INLINE_HEIGHT),
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
        worker_main(
            agent,
            in_rx,
            out_tx,
            DoneSignal(done_tx, worker_done_flag),
            worker_cancel,
            worker_shutdown,
        )
    });

    // A sender the teardown below can reach after the loop (and with it the
    // session's own clone) is gone — whatever way the loop ended.
    let quit_tx = in_tx.clone();
    let window_note = cfg.limits.context_window_hint_line();
    let startup = Startup {
        model: &cfg.model,
        n_tools,
        initial_task: initial_task.as_deref(),
        auto_start,
        resume_rows,
        // The note for an unset model window. The TUI never shows stderr, so
        // it rides the welcome row — one dim line, not a warning wall.
        window_note: window_note.as_deref(),
    };
    let result = ui_loop(
        &mut term,
        cfg,
        in_tx,
        &out_rx,
        &worker_exited,
        cancel.clone(),
        startup,
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
    // Tell the worker to exit explicitly rather than through sender drops
    // alone: the session's clone went with the loop, the tty-disconnect
    // monitor keeps one until it notices `shutdown`, and a loop that ended on
    // an error never sent the `None` a quit key would have.
    let _ = quit_tx.send(None); // signal worker to exit
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

/// The event loop: drain → commit → repaint → at most one terminal event,
/// until a key handler returns [`Effect::Quit`]. Returns the number of queued
/// messages that were left behind.
fn ui_loop(
    term: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    cfg: &Config,
    in_tx: mpsc::Sender<Option<Input>>,
    out_rx: &mpsc::Receiver<UiMsg>,
    worker_exited: &AtomicBool,
    cancel: Arc<AtomicBool>,
    startup: Startup<'_>,
) -> Result<usize> {
    let mut session = Session::new(startup, in_tx, cancel);
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
            match out_rx.try_recv() {
                Ok(msg) => session.on_msg(msg, cfg, split_w),
                Err(mpsc::TryRecvError::Empty) => {
                    // The diag bridge (installed in `run` for the whole
                    // session) holds a sender clone, so the channel reports
                    // Empty — never Disconnected — even after the worker's
                    // own sender died with its thread. A worker gone mid-run
                    // (a panic in a debug build; release aborts the process)
                    // is therefore detected by the flag DoneSignal set on
                    // drop, not by channel state.
                    if session.run.phase.busy() && worker_exited.load(Ordering::Acquire) {
                        session.reset_after_worker_exit();
                    }
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Same reset as the worker-exited arm above. Defense in
                    // depth: unreachable while the diag bridge holds its
                    // sender clone, but kept so a future teardown that drops
                    // the bridge mid-session still resets the UI instead of
                    // hanging in "running".
                    session.reset_after_worker_exit();
                    break;
                }
            }
        }

        // Commit finalized turns into the terminal scrollback. A row is
        // "finalized" once it will never mutate again (see `view::commit`).
        view::commit(&mut session, term)?;

        // Render the live viewport — only when something changed (worker
        // event drained, key handled, first frame). While a run is in flight,
        // also redraw on a fast tick so the spinner animates and the
        // status-bar elapsed time keeps moving. Committed turns are already
        // in scrollback, so this redraws only the small bottom band, not the
        // whole history.
        let clock_tick =
            session.run.phase.busy() && session.last_draw.elapsed() >= Duration::from_millis(120);
        if session.dirty || clock_tick {
            session.dirty = false;
            session.last_draw = Instant::now();
            // The slash popup tracks the input text: any edit sets dirty, so
            // this refresh rides the same redraw.
            session
                .composer
                .popup
                .update_for(&session.composer.input.text(), session.run.phase.busy());
            term.draw(|f| view::draw(f, &mut session, cfg))?;
        }

        // Keyboard. Poll longer when idle: a keypress wakes the poll
        // immediately, so the longer timeout costs no input latency but
        // avoids spinning the loop while nothing is happening.
        let poll_ms = if session.run.phase.busy() { 60 } else { 250 };
        if event::poll(Duration::from_millis(poll_ms))? {
            let ev = event::read()?;
            // Any event may change the view (caret, popup, …).
            session.dirty = true;
            match ev {
                Event::Key(key) => {
                    if session.on_key(key, cfg) == Effect::Quit {
                        // Release the terminal on a frame without the
                        // completion popup (see `view::final_frame`).
                        view::final_frame(&mut session, term, cfg);
                        return Ok(session.queued.len());
                    }
                }
                Event::Paste(s) => session.on_paste(&s),
                // No mouse handler: with capture off, the terminal owns
                // wheel scroll and click-drag-select over the committed
                // scrollback — both "just work" on the real transcript, which
                // is the whole point of the inline model.
                _ => {}
            }
        }
    }
}
