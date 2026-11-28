//! The session: every piece of loop state, plus the transitions that do not
//! depend on where a change came from (the message queue, the run lifecycle).
//! The reducers are `impl Session` blocks next to their input: worker messages
//! in `events`, keys and slash commands in `keys` / `commands`, painting in
//! `view`. Nothing here touches the terminal, so all of it is testable with a
//! plain channel and constructed events.

use super::input::InputBuffer;
use super::markdown::MdCache;
use super::render::welcome_text;
use super::slash::SlashPopup;
use super::transcript::{Row, ToolRunning};
use super::worker::{CompactReport, Input};
use crate::agent::TodoItem;
use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::Instant;

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
    pub(super) fn busy(self) -> bool {
        matches!(self, RunPhase::Running | RunPhase::Cancelling)
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            RunPhase::Idle => "idle",
            RunPhase::Running => "running",
            RunPhase::Cancelling => "cancelling",
            RunPhase::Failed => "failed",
        }
    }
}

/// What `run` knows when the session starts; consumed by [`Session::new`].
pub(super) struct Startup<'a> {
    /// Model name for the welcome row.
    pub model: &'a str,
    /// Size of the live tool table, for the welcome row: what the model
    /// actually sees (see `run`).
    pub n_tools: usize,
    /// A task main already put in the agent's history (mirrored, never re-sent).
    pub initial_task: Option<&'a str>,
    /// The worker auto-runs when the session ends with a user message (an
    /// initial task, or a resumed session stopped mid-turn).
    pub auto_start: bool,
    /// A resumed session's rebuilt transcript; empty for a fresh one.
    pub resume_rows: Vec<Row>,
    /// The one-line note for an unset model window
    /// (`Limits::context_window_hint_line`): a second line of the welcome
    /// row. `None` once the window is set.
    pub window_note: Option<&'a str>,
}

/// Everything that only exists while a run is in flight, or says how the last
/// one ended. One unit with one job: answer "what is the agent doing right
/// now" for the live viewport.
pub(super) struct RunState {
    pub phase: RunPhase,
    // Start of the current run (one user message → possibly many turns), for
    // the status-bar elapsed time and the spinner frame. None when idle.
    pub run_start: Option<Instant>,
    // What the agent is doing right now ("thinking" / "writing" / "→ tool"),
    // shown in the busy status segment; fed by the event stream.
    pub activity: String,
    pub streaming: bool,
    // Transient "agent is thinking" marker: set at TurnStart (and when a
    // queued message is flushed into a new run), cleared by the first visible
    // output (text delta or tool call) and at run end. Render-only — the live
    // viewport renders a `◆ …` line from it; nothing is committed for it.
    pub thinking: bool,
    // Transient in-flight tool tracker (render-only, like `thinking`): the
    // ToolCall row commits immediately, so this placeholder is what keeps the
    // live zone showing WHICH tool is running and for how long.
    pub tool_running: ToolRunning,
    // Latest ToolInputDelta, render-only: argument fragments never enter rows.
    pub tool_building: Option<(String, Instant)>,
    // Name of the sub-agent whose text is currently accumulating into the
    // trailing Assistant row, so consecutive deltas from the same sub-agent
    // merge — and a different sub-agent (or the parent) starts a fresh row.
    pub sub_streaming_name: Option<String>,
    // Tag of the reasoning trace currently accumulating into a trailing
    // Row::Thinking ("" for the parent, the sub-agent's name for a child);
    // None when no trace is in flight. Same lifecycle as `streaming`.
    pub thinking_stream: Option<String>,
}

impl RunState {
    /// Whether a row is still being streamed into — the one predicate both the
    /// commit watermark (hold the open tail back) and the live viewport (show
    /// it) key off.
    pub(super) fn stream_open(&self) -> bool {
        self.streaming || self.sub_streaming_name.is_some() || self.thinking_stream.is_some()
    }

    /// The parent's answer and the reasoning trace are final. A child's open
    /// bubble is left alone: sibling tools may still be running.
    pub(super) fn end_parent_streams(&mut self) {
        self.streaming = false;
        self.thinking_stream = None;
    }

    /// Every child bubble and the reasoning trace are final; the parent's own
    /// answer stays open.
    pub(super) fn end_child_streams(&mut self) {
        self.sub_streaming_name = None;
        self.thinking_stream = None;
    }

    /// Nothing is streaming any more: the parent's answer, every child
    /// bubble and the trace are final. Where a run ends (finished, failed,
    /// worker gone) and where a tool call's arguments start arriving.
    pub(super) fn end_all_streams(&mut self) {
        self.streaming = false;
        self.sub_streaming_name = None;
        self.thinking_stream = None;
    }

    /// The parent is about to speak (text or trace): its open child bubble
    /// closes so the parent's words do not merge into it. Returns whether
    /// one was open — the parent's own answer must then start a fresh row
    /// too (it shares the Assistant row kind with the bubble).
    pub(super) fn close_sub_bubble(&mut self) -> bool {
        self.sub_streaming_name.take().is_some()
    }

    /// A run begins: busy, with its clock started. `activity` is the first
    /// thing the status line says it is doing; `thinking` says whether the
    /// `◆ …` placeholder shows (a turn waits for the model, a compact for
    /// its summary).
    pub(super) fn start(&mut self, activity: &str, thinking: bool) {
        self.phase = RunPhase::Running;
        self.run_start = Some(Instant::now());
        self.activity = activity.into();
        self.thinking = thinking;
    }

    /// The run ended and nothing takes over: idle, no clock, no placeholder.
    pub(super) fn idle(&mut self) {
        self.phase = RunPhase::Idle;
        self.run_start = None;
        self.thinking = false;
    }

    /// The run ended badly and nothing takes over. Failed is idle for
    /// editing and submission; what it changes is the title, the status
    /// segment and the held queue (see the reducers).
    pub(super) fn fail(&mut self) {
        self.phase = RunPhase::Failed;
        self.run_start = None;
        self.thinking = false;
    }

    /// Esc / Ctrl-C during a run. The status line and the input title say the
    /// stop is on its way and the kernel acknowledges at its next checkpoint —
    /// the run stays busy (a second press must not quit) until then. Nothing
    /// closes here: every open stream (the parent's answer, a child bubble,
    /// the reasoning trace) stays open, so the chunks that still land before
    /// the kernel stops append to the row they belong to. The transcript
    /// notices (`[cancelled]`, the discarded queue) follow the answer when
    /// the run ends — a `[cancelling…]` row at the keypress used to sit
    /// between an answer and its own last words, which then reappeared below
    /// it as a bare `◆` fragment.
    pub(super) fn begin_cancelling(&mut self) {
        self.phase = RunPhase::Cancelling;
        self.activity = "cancelling".into();
        self.tool_running.clear();
        self.tool_building = None;
    }
}

/// The input box and what orbits it: recall history, the slash popup, the
/// Ctrl-C quit arming.
pub(super) struct Composer {
    pub input: InputBuffer,
    // Input history (Alt+Up/Down to recall). `draft` holds the in-progress
    // text when the user starts browsing, so Down past the newest restores it.
    pub history: Vec<String>,
    pub hist_idx: Option<usize>,
    pub draft: String,
    // Slash-command completion popup: refreshed from the input text before
    // every draw, consulted by the key handler while open.
    pub popup: SlashPopup,
    // First idle Ctrl-C timestamp: a second press within CTRL_C_QUIT_WINDOW
    // actually quits; the first only shows the "press again" notice.
    pub ctrl_c_armed: Option<Instant>,
}

/// Session token totals for the status bar, summed from every bill the worker
/// reports (each turn's usage, tool bills, a manual compact's summary).
pub(super) struct Usage {
    pub in_tokens: u64,
    pub out_tokens: u64,
    // Prompt-cache write/read totals — shown in the status bar so the user can
    // see caching paying off (Anthropic reports both; OpenAI reports reads).
    pub cache_create: u64,
    pub cache_read: u64,
    // Estimated context-window occupancy after the latest API call, SET (not
    // summed) from each direct-request Usage event (tool-folded reports —
    // a tool's bill, not a measurement — leave it alone; see ctx_after_usage):
    // prompt tokens (incl. cached parts) plus the response just appended to
    // the history. Drives the status bar's `ctx` segment.
    pub ctx_tokens: u64,
}

/// All loop state. The UI loop owns exactly one and keeps no locals of its own;
/// every change goes through a method, so a test drives a transition exactly
/// the way the loop does.
pub(super) struct Session {
    // The full conversation, in append order. Finalized prefixes are committed
    // into the terminal scrollback via insert_before and never re-rendered; the
    // live viewport only ever shows the in-flight tail, so memory grows with
    // the session but per-frame work does not.
    pub rows: Vec<Row>,
    // Watermark: rows[0..committed] have already been flushed to scrollback and
    // are permanent. Only the open streaming tail is withheld; pending input
    // lives in a separate FIFO and cannot invalidate this index.
    pub committed: usize,
    // Whether the last commit ended on a blank line. A mid-stream batch
    // carries no end-of-history spacer, so the next batch has to open with
    // the gap its first row owes its predecessor (`row_gap`); one that ended
    // at the end of history already carries that blank.
    pub committed_tail_blank: bool,
    // Failed runs hold the FIFO until an explicit /resume. This survives a
    // separately submitted new prompt, which must not silently release it.
    pub queue_paused: bool,
    // Messages an in-flight cancel discarded (`on_cancel`). The queue is gone
    // the moment Esc is pressed; its notice waits for the run to end, when it
    // can follow the answer instead of cutting into it.
    pub cancel_dropped: Option<usize>,
    // Messages typed while a run is in flight, oldest first. This FIFO is the
    // ONLY queue truth; its preview is render-only and never enters `rows`.
    pub queued: VecDeque<String>,
    pub run: RunState,
    pub composer: Composer,
    pub usage: Usage,
    // The agent's current task plan, tracked via the `todo` tool. Shown live in
    // the status bar (a `plan done/total` segment) so progress is visible
    // without scrolling — the full checklist also lives in the most recent
    // `todo` tool-result row in the transcript. Updated on each TodoUpdate
    // (full-replacement semantics: each call sends the entire plan).
    pub plan: Vec<TodoItem>,
    // Per-row markdown render cache. Only the in-flight streaming row is ever
    // re-rendered in the live viewport, so this stays small; committed rows are
    // rendered once at commit time.
    pub md_cache: MdCache,
    // Dirty flag for the render pass: set by any worker message or terminal
    // event; while clear (and not busy) the loop skips term.draw entirely.
    pub dirty: bool,
    pub last_draw: Instant,
    // Full cwd path for the status bar's right-aligned segment — computed
    // once, not per frame.
    pub cwd_path: String,
    /// The worker's inbox. `None` on it means "quit".
    pub in_tx: mpsc::Sender<Option<Input>>,
    /// Raised by Esc / Ctrl-C during a run; the agent checks it at each turn
    /// boundary. This is the RECOVERABLE per-turn interrupt — the next turn
    /// reuses the same MCP server child, so it must never tear it down.
    pub cancel: Arc<AtomicBool>,
}

impl Session {
    pub(super) fn new(
        startup: Startup<'_>,
        in_tx: mpsc::Sender<Option<Input>>,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        // Full cwd path for the status bar's right-aligned segment — computed
        // once, not per frame.
        let cwd_path: String = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut session = Self {
            rows: Vec::new(),
            committed: 0,
            committed_tail_blank: true,
            queue_paused: false,
            cancel_dropped: None,
            queued: VecDeque::new(),
            run: RunState {
                phase: RunPhase::Idle,
                run_start: None,
                activity: String::new(),
                streaming: false,
                thinking: false,
                tool_running: ToolRunning::new(),
                tool_building: None,
                sub_streaming_name: None,
                thinking_stream: None,
            },
            composer: Composer {
                input: InputBuffer::new(),
                history: Vec::new(),
                hist_idx: None,
                draft: String::new(),
                popup: SlashPopup::new(),
                ctrl_c_armed: None,
            },
            usage: Usage {
                in_tokens: 0,
                out_tokens: 0,
                cache_create: 0,
                cache_read: 0,
                ctx_tokens: 0,
            },
            plan: Vec::new(),
            md_cache: MdCache::new(),
            dirty: true, // first frame must paint
            last_draw: Instant::now(),
            cwd_path,
            in_tx,
            cancel,
        };

        session.rows.push(Row::sys_head(welcome_text(
            startup.model,
            startup.n_tools,
            startup.window_note,
        )));
        if startup.resume_rows.is_empty() {
            // Mirror the initial task (already in the agent's history) so the user
            // sees what's running. Do NOT re-send it over in_tx: main has already
            // placed it in the agent's messages, and a second send would duplicate
            // the task and create two adjacent user messages (Anthropic rejects
            // that 400). The worker starts the run itself.
            if let Some(task) = startup.initial_task {
                session.rows.push(Row::User(task.to_string()));
            }
        } else {
            // Resumed session: the transcript already ends with the final user
            // message (including a --task folded into it by main), so mirroring
            // initial_task here would duplicate it. The rebuilt rows are ordinary
            // rows — the first commit pass flushes them into scrollback.
            session.rows.push(Row::sys_info(format!(
                "[resumed session — {} transcript rows restored]",
                startup.resume_rows.len()
            )));
            session.rows.extend(startup.resume_rows);
        }
        // The worker auto-runs when the session ends with a user message (an
        // initial task, or a resumed session stopped mid-turn). Show "running" so
        // the status bar is honest; further submissions queue behind this run.
        if startup.auto_start {
            // Busy from the first frame; no activity yet and no `◆ …`
            // placeholder — the first event (TurnStart) brings those.
            session.run.start("", false);
        }
        session
    }

    /// A fresh idle session wired to `in_tx` — the starting point of the
    /// loop-level tests (events / keys → rows + state).
    #[cfg(test)]
    pub(super) fn for_test(in_tx: mpsc::Sender<Option<Input>>, cancel: Arc<AtomicBool>) -> Self {
        Self::new(
            Startup {
                model: "test-model",
                n_tools: 0,
                initial_task: None,
                auto_start: false,
                resume_rows: Vec::new(),
                window_note: None,
            },
            in_tx,
            cancel,
        )
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

impl Session {
    /// Pop the oldest queued message and start its run. The pending FIFO never
    /// inserted ghost Rows, so no committed index can move here; the real User
    /// row is appended only at the send boundary, preserving true conversation
    /// order.
    pub(super) fn flush_next_queued(&mut self) -> Flush {
        let Some(text) = self.queued.pop_front() else {
            return Flush::Empty;
        };
        self.rows.push(Row::User(text.clone()));
        if self.in_tx.send(Some(Input::Text(text))).is_ok() {
            Flush::Sent
        } else {
            self.rows.push(Row::sys_error(
                "[worker stopped — queued message not delivered; type /quit to exit]",
            ));
            Flush::Undeliverable
        }
    }

    /// Apply a [`flush_next_queued`] outcome to the run state. Sent starts the
    /// next run with NO idle gap (Ctrl-C must cancel, not quit); everything
    /// else idles.
    pub(super) fn apply_flush(&mut self, flush: Flush) {
        match flush {
            Flush::Sent => self.run.start("thinking", true),
            Flush::Undeliverable => self.run.fail(),
            Flush::Empty => self.run.idle(),
        }
    }

    /// The notice owed for the queue an Esc discarded — once the run is over,
    /// so it lands after the answer instead of inside it (see `on_cancel`).
    pub(super) fn push_cancel_dropped(&mut self) {
        if let Some(n) = self.cancel_dropped.take() {
            self.rows
                .push(Row::sys_warn(format!("[dropped {n} queued message(s)]")));
        }
    }

    /// Reset the run state after the worker thread died mid-run: drop the UI out
    /// of "running" (the RunFinished that would reset it is never coming),
    /// surface the death, and drop the queued messages — nothing can deliver
    /// them anymore. Shared by the event loop's worker-exited and channel-
    /// disconnected arms so the two paths cannot drift. Returns whether anything
    /// changed; a change also marks the session dirty, so the next repaint
    /// shows it without the caller having to remember.
    pub(super) fn reset_after_worker_exit(&mut self) -> bool {
        let mut changed = false;
        if self.run.phase.busy() {
            self.run.end_all_streams();
            self.run.fail();
            self.run.activity.clear();
            self.run.tool_running.clear();
            self.rows.push(Row::sys_error(
                "[worker stopped unexpectedly — type /quit to exit]",
            ));
            changed = true;
        }
        // No worker can accept these messages; the FIFO contains no transcript
        // ghosts, so dropping it cannot disturb committed row indices. The
        // count an in-flight cancel already discarded joins the same notice.
        let dropped = self
            .queued
            .len()
            .saturating_add(self.cancel_dropped.take().unwrap_or(0));
        if dropped > 0 {
            self.rows.push(Row::sys_warn(format!(
                "[dropped {dropped} queued message(s)]"
            )));
            changed = true;
        }
        self.queued.clear();
        self.dirty |= changed;
        changed
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
