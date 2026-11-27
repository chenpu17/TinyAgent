//! The worker side: the thread that owns the agent, and the messages crossing
//! to and from it (`UiMsg` up, `Input` down). The UI never touches the agent —
//! every run, `/clear`, `/context` and `/compact` is a message handled here.

use crate::agent::{Agent, AgentEvent};
use crate::provider::Block;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};

#[derive(Debug)]
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
#[derive(Debug, PartialEq, Eq)]
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
pub(super) struct DoneSignal(
    pub(super) mpsc::Sender<()>,
    pub(super) Arc<std::sync::atomic::AtomicBool>,
);

impl Drop for DoneSignal {
    fn drop(&mut self) {
        self.1.store(true, std::sync::atomic::Ordering::Release);
        let _ = self.0.send(());
    }
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

/// The worker thread's body: own the agent for the whole session, run one turn
/// per submitted message, answer the side commands. Split out of the
/// `thread::spawn` closure in `run` so the protocol reads top to bottom and the
/// stop signals are plain parameters rather than captures.
pub(super) fn worker_main(
    mut agent: Agent,
    in_rx: mpsc::Receiver<Option<Input>>,
    out_tx: mpsc::Sender<UiMsg>,
    done: DoneSignal,
    cancel: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    // Signal completion on ANY exit (normal loop end or early return) via
    // Drop, so the quit path can join within a bounded wait and the UI loop
    // can detect a mid-run death (see `worker_exited` in `run`).
    let _done = done;
    agent.set_cancel(cancel);
    // Arm the MCP shutdown-reaper on the session-end flag (NOT cancel): a
    // per-turn Esc leaves the server alive; only a real quit kills it.
    agent.set_shutdown(shutdown);

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
}
