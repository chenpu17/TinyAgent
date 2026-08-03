use crate::config::{Config, Limits};
use crate::compress;
use crate::provider::{Block, ChatEvent, ChatRequest, Message, Provider, Role, ToolDef, Usage};
use crate::tools::{Tool, ToolCtx, ToolOutput};
use anyhow::Result;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What triggered a history compression: the per-turn preflight estimate
/// crossing the configured threshold, or overflow recovery after the
/// provider rejected a request as too long for the context window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressTrigger {
    Preflight,
    Overflow,
}

/// Events emitted during a run; frontends (headless printer / TUI) consume them.
pub enum AgentEvent {
    TurnStart(u32),
    TextDelta(String),
    ToolCall { name: String, input: Value },
    ToolResult { name: String, is_error: bool, content: String },
    /// Token usage reported by the provider for the latest chat request.
    /// Best-effort: emitted only when the provider actually reported usage
    /// (e.g. the stream wasn't cut off before the usage frame). The cache_*
    /// fields are 0 for providers/requests with no prompt caching.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_creation_input_tokens: u64,
        cache_read_input_tokens: u64,
    },
    /// History was compressed: how many middle messages were dropped,
    /// how many remain, and which path triggered it.
    Compressed {
        dropped: usize,
        kept: usize,
        trigger: CompressTrigger,
    },
    /// A non-fatal problem worth surfacing (session save failed, compression
    /// disabled after an error, …). Frontends decide how to show it — the
    /// headless printer writes stderr, the TUI renders a system line. NEVER
    /// eprintln from the agent: in TUI mode the worker thread shares the
    /// terminal with ratatui's raw-mode rendering and would corrupt the
    /// screen.
    Warning(String),
    /// An event forwarded from a named sub-agent (the `task` tool). Carries the
    /// sub-agent's identity so a harness/UI can show concurrent sub-agent work
    /// and tell interleaved streams apart. The inner `event` is the child's own
    /// `AgentEvent` (TextDelta, ToolCall, …) — recurse to render it.
    SubAgent { name: String, event: Box<AgentEvent> },
    Done { stop_reason: String },
}

#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub name: String,
    pub input: Value,
    /// The tool's result text (what read_file returned, shell stdout, …) — kept
    /// so the structured result file is self-contained for verification, not
    /// just the model's summary of it.
    pub output: String,
    pub is_error: bool,
}

pub struct RunOutcome {
    pub final_text: String,
    pub turns: u32,
    pub stop_reason: String,
    pub tool_calls: Vec<ToolCallRecord>,
    /// Token usage summed across every chat request in this run (best-effort:
    /// only the requests whose stream reached the usage frame). The cache_*
    /// fields accumulate prompt-cache writes/reads so the reported cost stays
    /// honest when caching is on (Anthropic reports both; OpenAI reports reads).
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub elapsed: Duration,
}

pub struct Agent {
    provider: Box<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    tool_defs: Vec<ToolDef>,
    messages: Vec<Message>,
    system: Option<String>,
    model: String,
    max_tokens: u32,
    limits: Limits,
    ctx: ToolCtx,
    session_path: Option<std::path::PathBuf>,
    /// Set by the frontend (e.g. TUI Esc) to stop at the next turn boundary.
    cancel: Option<Arc<AtomicBool>>,
    /// One compression failure disables further attempts for this run,
    /// so a persistent error doesn't retry (and warn) every single turn.
    compress_failed: bool,
    /// Running token totals for the current run (reset at run_impl entry).
    /// Kept on self — not a local — so a sub-agent that fails MID-run can
    /// still bill the turns it already burned (honest cost) via usage_so_far.
    usage: Usage,
    /// Pre-serialized size of the tool definitions (name + description +
    /// schema JSON). tool_defs never changes after construction, so
    /// estimate_tokens would otherwise re-serialize every schema on EVERY
    /// turn's preflight check.
    tool_schema_chars: usize,
}

impl Agent {
    pub fn new(
        cfg: &Config,
        provider: Box<dyn Provider>,
        tools: Vec<Box<dyn Tool>>,
        initial: Vec<Message>,
        ctx: ToolCtx,
    ) -> Self {
        let tool_defs: Vec<ToolDef> = tools.iter().map(|t| t.def()).collect();
        let tool_schema_chars = tool_defs
            .iter()
            .map(|t| t.name.len() + t.description.len() + t.schema.to_string().len())
            .sum();
        Self {
            provider,
            tools,
            tool_defs,
            messages: initial,
            system: cfg.system_prompt.clone(),
            model: cfg.model.clone(),
            max_tokens: cfg.max_tokens,
            limits: cfg.limits.clone(),
            ctx,
            session_path: None,
            cancel: None,
            compress_failed: false,
            usage: Usage::default(),
            tool_schema_chars,
        }
    }

    /// Attach a cancellation flag; checked at every turn boundary and shared
    /// with tools (via ToolCtx) so a running shell can be interrupted too,
    /// not only the streaming read.
    pub fn set_cancel(&mut self, flag: Arc<AtomicBool>) {
        self.ctx.cancel = Some(flag.clone());
        self.cancel = Some(flag);
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Relaxed))
    }

    /// The cancel flag to hand a chat request: the shared one if set, else a
    /// dummy never-set flag (so the SSE idle-timeout still applies when nobody
    /// can cancel — headless without Ctrl-C wiring).
    fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel
            .clone()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)))
    }

    /// Token totals accumulated so far in the current/last run. A sub-agent
    /// that fails mid-run bills these to the parent — the turns it completed
    /// were real billed calls even though the run errored (honest cost).
    pub(crate) fn usage_so_far(&self) -> Usage {
        self.usage
    }

    /// Fold one request's usage into the run totals and emit the per-request
    /// Usage event. Single site for the accumulate+emit pair — the four call
    /// sites (preflight/forced compression, main chat, tool-billed usage) used
    /// to each duplicate it, and the Some(Some) vs Some(None) semantics had to
    /// be re-explained at every copy.
    fn fold_usage(&mut self, events: &mut dyn FnMut(AgentEvent), u: &Usage) {
        self.usage.input_tokens += u.input_tokens;
        self.usage.output_tokens += u.output_tokens;
        self.usage.cache_creation_input_tokens += u.cache_creation_input_tokens;
        self.usage.cache_read_input_tokens += u.cache_read_input_tokens;
        events(AgentEvent::Usage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_creation_input_tokens: u.cache_creation_input_tokens,
            cache_read_input_tokens: u.cache_read_input_tokens,
        });
    }

    /// Test-only view of the trailing user message's tool_result contents —
    /// used to assert N tool_use ↔ N tool_result pairing survives a mid-batch
    /// cancel (cancelled tools get a synthesized "[cancelled by user]" result
    /// in the message history, even though they aren't recorded as executed).
    #[cfg(test)]
    pub(crate) fn trailing_tool_result_contents(&self) -> Vec<String> {
        self.messages
            .last()
            .filter(|m| m.role == Role::User)
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        Block::ToolResult { content, .. } => Some(content.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether the last message is a user message. The TUI uses this to
    /// decide whether to kick off an initial run — an initial task baked
    /// in by main, or a resumed session that stopped mid-turn at a
    /// tool_result — instead of waiting for the user to type something.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub fn ends_with_user(&self) -> bool {
        self.messages.last().is_some_and(|m| m.role == Role::User)
    }

    /// Enable per-turn session persistence to the given file.
    pub fn enable_session(&mut self, path: std::path::PathBuf) {
        self.session_path = Some(path);
    }

    /// Persist the session, returning a warning message on failure instead of
    /// printing — the agent must never write to stderr itself (in TUI mode the
    /// worker thread shares the terminal with ratatui). Callers with an event
    /// channel forward it as `AgentEvent::Warning`.
    fn save_session(&self) -> Option<String> {
        if let Some(path) = &self.session_path {
            if let Err(e) = crate::session::save(path, &self.model, &self.messages) {
                return Some(format!("session save failed: {e:#}"));
            }
        }
        None
    }

    /// Append a user message (multi-turn / TUI follow-ups). Returns a warning
    /// if the session file couldn't be persisted.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub fn push_user(&mut self, blocks: Vec<Block>) -> Option<String> {
        append_user_turn(&mut self.messages, blocks);
        self.save_session()
    }

    /// Drop the conversation transcript (`/clear`). Keeps config, tools, and
    /// system prompt — only messages are reset, and the session file is
    /// rewritten so a restart doesn't restore the cleared chat. Returns a
    /// warning if the session file couldn't be persisted.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub fn clear_messages(&mut self) -> Option<String> {
        self.messages.clear();
        self.save_session()
    }

    /// Run the loop until the model stops calling tools, or a limit is hit.
    /// Resets a stale cancel flag from a previous run (this `Agent` may be
    /// reused across runs — TUI follow-ups, harness retries — so a Ctrl-C that
    /// ended the last run must not bleed into this one).
    pub fn run(&mut self, events: &mut dyn FnMut(AgentEvent)) -> Result<RunOutcome> {
        self.run_impl(events, /* reset_cancel */ true)
    }

    /// Drive this agent as a `task` sub-agent's child. Like `run`, but does NOT
    /// reset the shared cancel flag at entry. The child inherits the parent's
    /// cancel `Arc`; resetting here would let a sibling sub-agent starting
    /// slightly later in a parallel `task` batch clear a Ctrl-C just signalled
    /// on the shared flag — dropping the cancel for the whole batch. That is the
    /// exact race CLAUDE.md records `batch::run` dodging by not sharing the flag
    /// with its workers. Skipping the reset is safe: the child is freshly built
    /// per call (no stale cancel to clear), and the batch gate guarantees the
    /// flag is false when workers spawn — so any `true` the child later observes
    /// is a real cancel it must honour, never stale state.
    pub(crate) fn run_as_subagent(
        &mut self,
        events: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        self.run_impl(events, /* reset_cancel */ false)
    }

    fn run_impl(
        &mut self,
        events: &mut dyn FnMut(AgentEvent),
        reset_cancel: bool,
    ) -> Result<RunOutcome> {
        let start = Instant::now();
        // Each top-level run gets a fresh compression budget. In the TUI the
        // same Agent is reused across follow-ups; without this reset, one
        // transient compression failure would disable it for the rest of the
        // session and the history would grow until it hit the model's limit.
        self.compress_failed = false;
        // Fresh usage totals per run (this Agent may be reused across runs —
        // TUI follow-ups, harness retries).
        self.usage = Usage::default();
        // Reset any stale cancel from a previous run: cancel is level-triggered
        // and never auto-clears elsewhere, so without this a Ctrl-C that ended
        // the last turn would cancel this one before it starts. Only the
        // top-level `run` resets — a `task` sub-agent (run_as_subagent) shares
        // the parent's flag and must NOT reset, or a sibling starting later in a
        // parallel batch would race a just-signalled Ctrl-C (see run_as_subagent).
        if reset_cancel {
            if let Some(c) = &self.cancel {
                c.store(false, Ordering::Relaxed);
            }
        }
        // Overall run deadline. Each chat is capped to the time left so a
        // single stuck request can't outlive the run budget — which is
        // otherwise only enforced at turn boundaries.
        let deadline = (self.limits.timeout_secs > 0)
            .then(|| start + Duration::from_secs(self.limits.timeout_secs));
        let mut turns = 0u32;
        let mut final_text = String::new();
        let mut records: Vec<ToolCallRecord> = Vec::new();
        let mut stop_reason: String;

        loop {
            if self.cancelled() {
                stop_reason = "cancelled".into();
                break;
            }
            if turns >= self.limits.max_turns {
                stop_reason = "max_turns_reached".into();
                break;
            }
            // Compare against the exact deadline, not as_secs(): the truncated
            // check let a turn start with milliseconds of budget left, and that
            // request then failed as a timeout Err (exit 1, no result) instead
            // of a clean "timeout" stop_reason with the transcript intact. The
            // turns>0 floor: a fresh request with <1s of budget can't succeed,
            // so stop cleanly instead. The first turn always runs — it starts
            // with (nearly) the full budget by construction (deadline is None
            // when timeout_secs == 0, i.e. "no timeout").
            if let Some(d) = deadline {
                let now = Instant::now();
                if now >= d
                    || (turns > 0 && d.saturating_duration_since(now) < Duration::from_secs(1))
                {
                    stop_reason = "timeout".into();
                    break;
                }
            }
            turns += 1;
            events(AgentEvent::TurnStart(turns));
            // Preflight compression: if the history is large enough that this
            // request would blow the window, summarize first. The summary
            // request is a real billed call; fold its usage into the run totals
            // so result.json / TUI report honest costs. compress returns
            // Option<Option<Usage>>: outer None = no compression ran; inner
            // None = compression ran but the provider reported no usage. We only
            // count/emit on Some(Some): a usage-less summary contributes nothing
            // and must not emit a noise Usage{0,0} event into the NDJSON stream
            // / TUI status.
            if let Some(Some(u)) = self.compress(events, deadline, /*force*/ false) {
                self.fold_usage(events, &u);
            }

            // Overflow recovery (OpenCode semantics): if the provider rejects
            // this request as too long for the context window, force ONE
            // compression and retry the turn on the shorter history; a second
            // overflow is a real error. The request borrows &self.messages, so
            // it lives in an inner block whose end releases that borrow before
            // the (mutable) compress call in the recovery arm.
            let mut overflow_recovered = false;
            let resp = loop {
                let r = {
                    // Safety net: fold any adjacent same-role messages (e.g. a
                    // resumed session that ends on two user messages, or a
                    // hand-edited history) into one before sending — Anthropic
                    // rejects consecutive user messages with a 400. Idempotent,
                    // so re-running it after a compression-folded history (still
                    // strictly alternating) is a no-op.
                    normalize_adjacent(&mut self.messages);

                    let req = ChatRequest {
                        system: self.system.as_deref(),
                        messages: &self.messages,
                        tools: &self.tool_defs,
                        model: &self.model,
                        max_tokens: self.max_tokens,
                        per_request_timeout: deadline
                            .map(|d| d.saturating_duration_since(Instant::now())),
                        // Shared cancel flag: the streaming reader checks it
                        // between chunks, so Esc/Ctrl-C can interrupt a stuck
                        // request mid-read (not only at the next turn boundary).
                        cancel: self.cancel_flag(),
                        // The main turn benefits from prompt caching (re-reads
                        // the stable prefix across turns); one-off requests opt
                        // out.
                        cache: true,
                    };
                    self.provider.chat(&req, &mut |e| match e {
                        ChatEvent::TextDelta(t) => events(AgentEvent::TextDelta(t)),
                    })
                }; // req dropped here → &self.messages borrow released
                // Baseline AFTER normalize_adjacent, so a same-role merge that
                // shrank the history isn't mistaken for a successful compression
                // fold — this gate means "did compression actually shrink it?".
                let before = self.messages.len();
                match r {
                    Ok(resp) => break resp,
                    Err(e) if !overflow_recovered && compress::is_context_overflow(&e) => {
                        // Need a compressible head and a non-failed compressor;
                        // checked in the body (not the guard) to keep `self`
                        // borrows out of the match guard.
                        let can_compress = !self.compress_failed
                            && compress::find_safe_cut(
                                &self.messages,
                                self.limits.keep_recent_messages,
                            )
                            .is_some();
                        if can_compress {
                            overflow_recovered = true;
                            // The server proved it's too long: force one
                            // compression regardless of the size threshold, and
                            // fold its usage into the run totals (honest cost).
                            if let Some(Some(u)) =
                                self.compress(events, deadline, /*force*/ true)
                            {
                                self.fold_usage(events, &u);
                            }
                            // Only retry if compression actually shrank the
                            // history; otherwise there's nothing more we can do.
                            if self.messages.len() < before {
                                continue;
                            }
                        }
                        return Err(e);
                    }
                    Err(e) => return Err(e),
                }
            };
            if let Some(u) = resp.usage {
                self.fold_usage(events, &u);
            }

            stop_reason = resp.stop_reason.clone();
            let has_tool_use = resp
                .blocks
                .iter()
                .any(|b| matches!(b, Block::ToolUse { .. }));
            // The result summary keeps only the last turn's text — tool-loop
            // chatter from earlier turns is transcript, not conclusion.
            let mut turn_text = String::new();
            for b in &resp.blocks {
                if let Block::Text(t) = b {
                    if !turn_text.is_empty() {
                        turn_text.push('\n');
                    }
                    turn_text.push_str(t);
                }
            }
            if !turn_text.is_empty() {
                final_text = turn_text;
            }
            // An empty assistant turn (e.g. a content-filtered response that
            // yields zero blocks) must NOT be persisted: an empty assistant
            // message makes every later request 400 on strict APIs (Anthropic
            // requires each message to carry >=1 content block), and once saved
            // it does so until the session is cleared. final_text is already
            // empty for this turn and has_tool_use is necessarily false, so just
            // stop here with the provider's stop_reason.
            if resp.blocks.is_empty() {
                break;
            }
            self.messages.push(Message::assistant(resp.blocks.clone()));
            if let Some(w) = self.save_session() {
                events(AgentEvent::Warning(w));
            }

            // A cancelled / idle-timeout / truncated / max_tokens stream may
            // leave a half-delivered tool_use whose input is partial or _raw.
            // OpenAI maps finish_reason "length" → "max_tokens"; when the
            // budget runs out mid tool-call arguments the truncated JSON fails
            // to parse and falls back to a _raw sentinel — executing that is
            // garbage. (Both providers now yield _raw leniently; on a COMPLETE
            // stream a _raw input goes to the tool layer, which reports the
            // bad input back to the model so it can retry — only aborted
            // streams are stripped here.)
            // Drop such tool_use blocks; also remove them from the saved
            // assistant message so a tool_use with no matching tool_result
            // isn't rejected by strict APIs on the next request. Keep whatever
            // partial text arrived so the transcript reflects what the user
            // saw. (stop_reason is still reported as-is below.)
            let aborted = self.cancelled()
                || matches!(
                    resp.stop_reason.as_str(),
                    "cancelled" | "stream_idle" | "stream_truncated" | "max_tokens"
                );
            if aborted {
                // If the abort is because a cancel arrived in the window between
                // chat() returning and this check (e.g. during save_session
                // above), stop_reason is still the provider's value (e.g.
                // "tool_use"/"end_turn") — which would map to the wrong exit
                // code (1 or 0 instead of 130). The cancel is the real reason
                // here, so override it. (The other abort source — stop_reason
                // itself being max_tokens/stream_truncated — leaves it as-is.)
                if self.cancelled() {
                    stop_reason = "cancelled".into();
                }
                let kept: Vec<Block> = resp
                    .blocks
                    .iter()
                    .filter(|b| !matches!(b, Block::ToolUse { .. }))
                    .cloned()
                    .collect();
                // Replace the just-pushed message with the filtered one (or
                // drop it entirely if only a partial tool_use arrived).
                match self.messages.last_mut() {
                    Some(m) if m.role == Role::Assistant => {
                        if kept.is_empty() {
                            self.messages.pop();
                        } else {
                            m.content = kept;
                        }
                    }
                    _ => {}
                }
                if let Some(w) = self.save_session() {
                    events(AgentEvent::Warning(w));
                }
                break;
            }

            if !has_tool_use {
                break; // end_turn / max_tokens without pending calls
            }

            // Execute each tool call, collect results into one user message.
            // Once cancelled (e.g. Ctrl-C during tool #1), stop running further
            // tools — but STILL emit a result for every remaining tool_use: an
            // assistant message carrying N tool_use blocks must be followed by
            // N tool_results, or the next request is rejected. (A non-cancel-
            // aware tool that was already in flight returns on its own; later
            // tools in the batch are short-circuited here.)

            // Collect tool_use blocks (in order) with stable ordinals — only
            // these produce results; the turn's text already streamed above.
            // Owned (not borrows into `resp`) so `.clone()` in the passes below
            // yields String/Value, and so `tool_uses` can be borrowed by the
            // scoped worker threads without entangling `resp`'s lifetime.
            let tool_uses: Vec<(String, String, Value)> = resp
                .blocks
                .iter()
                .filter_map(|b| match b {
                    Block::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect();

            // Per-slot outcome. `Cancelled` keeps N:N pairing (a result block
            // for every tool_use) without billing usage or recording a call —
            // matching the pre-parallel behavior. `Ran` is a real ToolOutput
            // (leaf run inline, or `task` run in the parallel batch).
            enum Slot {
                Cancelled,
                Ran(ToolOutput),
            }
            let mut slots: Vec<Slot> = (0..tool_uses.len()).map(|_| Slot::Cancelled).collect();
            // Ordinals of `task` calls deferred to the parallel batch.
            let mut task_slots: Vec<usize> = Vec::new();
            let mut batch_cancelled = false;

            // Inline leaf pass: run non-`task` tools one at a time (sequential —
            // avoids new races on shell/file), announce each LEAF call in block
            // order, and short-circuit on cancel (later slots stay Cancelled).
            // `task` calls are NOT announced here — their ToolCall is emitted in
            // the batch block below, AFTER the cancel gate, so a task gated off
            // by a later leaf tripping cancel never dangles a ToolCall without a
            // matching ToolResult on the UI.
            for (i, (_id, name, input)) in tool_uses.iter().enumerate() {
                if batch_cancelled || self.cancelled() {
                    batch_cancelled = true;
                    continue; // leaves the default Slot::Cancelled
                }
                if name.as_str() == "task" {
                    task_slots.push(i); // ToolCall announced + run in the batch below
                } else {
                    events(AgentEvent::ToolCall {
                        name: name.clone(),
                        input: input.clone(),
                    });
                    let output = {
                        let mut fwd = |ev: AgentEvent| events(ev);
                        run_tool(&self.tools, &self.tool_defs, name, input, &self.ctx, &mut fwd)
                    };
                    slots[i] = Slot::Ran(output);
                }
            }

            // Parallel `task` batch: each `task` call runs on its own scoped
            // thread (borrowing the read-only tool list + ctx, both Sync), and
            // forwards its child events through an mpsc funnel the parent drains
            // live — so concurrent sub-agents interleave on screen, tagged by
            // name. Leaf tools stay sequential (above); only `task` is fanned
            // out — it's the one worth parallelizing, and the only non-leaf
            // `run_with_emit` impl. Cancel: gate before spawning (a cancel that
            // hit during the inline pass, or one already set, marks the batch
            // cancelled and skips it); in-flight children self-terminate via the
            // shared cancel flag they inherit, returning partial/cancelled
            // results rather than synthesised ones.
            if !task_slots.is_empty() && !batch_cancelled && !self.cancelled() {
                // Announce the task calls in block order NOW that the batch is
                // committed to run — emitting them in the inline pass (before the
                // gate) would leave a ToolCall with no matching ToolResult if a
                // later leaf tripped cancel and gated this batch off.
                for &i in &task_slots {
                    events(AgentEvent::ToolCall {
                        name: tool_uses[i].1.clone(),
                        input: tool_uses[i].2.clone(),
                    });
                }
                let (tx, rx) = std::sync::mpsc::channel::<AgentEvent>();
                let tools = &self.tools;
                let defs = &self.tool_defs;
                let ctx = &self.ctx;
                std::thread::scope(|s| {
                    // Spawn every task worker BEFORE joining any, so they run
                    // concurrently. Each owns its input/name + a sender clone,
                    // and only ever touches `tx` (never `events`).
                    let mut handles: Vec<std::thread::ScopedJoinHandle<ToolOutput>> =
                        Vec::with_capacity(task_slots.len());
                    for &i in &task_slots {
                        let name = tool_uses[i].1.clone();
                        let input = tool_uses[i].2.clone();
                        let tx = tx.clone();
                        handles.push(s.spawn(move || {
                            let mut emit = |ev: AgentEvent| {
                                let _ = tx.send(ev);
                            };
                            run_tool(tools, defs, &name, &input, ctx, &mut emit)
                        }));
                    }
                    drop(tx); // workers hold the last clones; rx ends when they drop
                    // Forward child events to the parent sink as they arrive
                    // (interleaved across parallel sub-agents, told apart by name).
                    while let Ok(ev) = rx.recv() {
                        events(ev);
                    }
                    // Join in spawn order and place each result at its ordinal.
                    for (pos, h) in handles.into_iter().enumerate() {
                        let output = h
                            .join()
                            .unwrap_or_else(|_| ToolOutput::error("task worker panicked"));
                        slots[task_slots[pos]] = Slot::Ran(output);
                    }
                });
            }

            // Assemble results in block order: a result block for every tool_use
            // (N:N), but records / ToolResult events / usage fold only for tools
            // that actually ran. A `task` call's usage (the child's honest cost)
            // is folded here exactly like a leaf tool's billed usage.
            let mut results: Vec<Block> = Vec::with_capacity(tool_uses.len());
            for (i, (id, name, input)) in tool_uses.iter().enumerate() {
                match &slots[i] {
                    Slot::Cancelled => results.push(Block::ToolResult {
                        tool_use_id: id.clone(),
                        content: "[cancelled by user]".into(),
                        image: None,
                        is_error: true,
                    }),
                    Slot::Ran(output) => {
                        if let Some(u) = output.usage {
                            self.fold_usage(events, &u);
                        }
                        records.push(ToolCallRecord {
                            name: name.clone(),
                            input: input.clone(),
                            output: output.content.clone(),
                            is_error: output.is_error,
                        });
                        events(AgentEvent::ToolResult {
                            name: name.clone(),
                            is_error: output.is_error,
                            content: output.content.clone(),
                        });
                        results.push(Block::ToolResult {
                            tool_use_id: id.clone(),
                            content: output.content.clone(),
                            image: output.image.clone().map(Box::new),
                            is_error: output.is_error,
                        });
                    }
                }
            }
            self.messages.push(Message::user(results));
            if let Some(w) = self.save_session() {
                events(AgentEvent::Warning(w));
            }
        }

        events(AgentEvent::Done {
            stop_reason: stop_reason.clone(),
        });
        Ok(RunOutcome {
            final_text,
            turns,
            stop_reason,
            tool_calls: records,
            input_tokens: self.usage.input_tokens,
            output_tokens: self.usage.output_tokens,
            cache_creation_input_tokens: self.usage.cache_creation_input_tokens,
            cache_read_input_tokens: self.usage.cache_read_input_tokens,
            elapsed: start.elapsed(),
        })
    }
}

/// Look up a tool by name and run it through `run_with_emit` (so the `task`
/// sub-agent can stream its child events into `emit`). A missing tool or a tool
/// Err becomes a `ToolOutput::error` rather than aborting the run — the result
/// is still paired N:N with the tool_use that triggered it. Shared by the
/// sequential leaf pass and the parallel `task` batch.
///
/// Lookup goes through `defs` (the cached ToolDefs, built in the same order as
/// `tools`): matching on `tool.def().name` would deep-clone every probed
/// tool's name/description/schema on EVERY call.
fn run_tool(
    tools: &[Box<dyn Tool>],
    defs: &[ToolDef],
    name: &str,
    input: &Value,
    ctx: &ToolCtx,
    emit: &mut dyn FnMut(AgentEvent),
) -> ToolOutput {
    match defs.iter().position(|d| d.name == name) {
        Some(i) => match tools[i].run_with_emit(input, ctx, emit) {
            Ok(o) => o,
            Err(e) => ToolOutput::error(format!("{e:#}")),
        },
        None => ToolOutput::error(format!("unknown tool: {name}")),
    }
}

pub(crate) fn preview(s: &str) -> String {
    const MAX: usize = 200;
    // Single pass: take MAX chars, then test whether anything remained. The
    // old `.chars().count() > MAX` walked the WHOLE string just to decide
    // truncation — O(n) per call, and the renderer calls this every frame for
    // every collapsed tool row.
    let mut it = s.chars();
    let head: String = it.by_ref().take(MAX).collect();
    if it.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Merge adjacent messages that share a role into one. Anthropic rejects two
/// consecutive user messages with a 400, and tool_use ↔ tool_result pairing is
/// simplest with strict role alternation. This is a safety net: the known
/// append paths already alternate, but a resumed/corrupt session or a future
/// code path could violate it. Block order is preserved (so N tool_use ↔
/// N tool_result pairing is unaffected) — only same-role neighbors fold.
fn normalize_adjacent(messages: &mut Vec<Message>) {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for m in messages.drain(..) {
        match out.last_mut() {
            Some(prev) if prev.role == m.role => prev.content.extend(m.content),
            _ => out.push(m),
        }
    }
    *messages = out;
}

/// Fold `blocks` into a trailing user message, or push a new one. Two adjacent
/// user messages make Anthropic reject the request with a 400, so a follow-up
/// must merge into the prior user turn — notably after a turn cancelled before
/// any assistant text (the assistant message is dropped, leaving history ending
/// in the prior user message this follow-up must merge into).
fn append_user_turn(messages: &mut Vec<Message>, blocks: Vec<Block>) {
    if messages.last().is_some_and(|m| m.role == Role::User) {
        if let Some(last) = messages.last_mut() {
            last.content.extend(blocks);
        }
    } else {
        messages.push(Message::user(blocks));
    }
}
impl Agent {
    /// Compress history if it's too large, summarizing the middle via the LLM
    /// and keeping only the head (+ folded summary) and recent tail. Failures
    /// degrade gracefully (warn and continue uncompressed).
    ///
    /// `force=true` skips the size threshold — used by overflow recovery, where
    /// the provider has just PROVEN the request is too long. The preflight path
    /// (`force=false`) gates on a token estimate against `max_context_tokens`
    /// when set, else the legacy char heuristic.
    ///
    /// Returns the summary request's token usage wrapped twice:
    /// - outer `None` — no compression ran (below threshold / out of budget /
    ///   nothing compressible when forced);
    /// - `Some(None)` — compression ran but the provider reported no usage
    ///   (count nothing, emit no Usage event);
    /// - `Some(Some(u))` — compression ran and billed `u` (count + emit).
    fn compress(
        &mut self,
        events: &mut dyn FnMut(AgentEvent),
        deadline: Option<Instant>,
        force: bool,
    ) -> Option<Option<Usage>> {
        // Threshold gate. force=true (overflow recovery) skips it — the provider
        // just proved the request is too long, so compress regardless of size.
        // Otherwise the trigger depends on config: a token estimate against
        // max_context_tokens when set, else the legacy char heuristic.
        if !force {
            let over = if self.limits.max_context_tokens > 0 {
                let est = compress::estimate_tokens(&self.messages, self.system.as_deref(), self.tool_schema_chars);
                // Trip at COMPACT_UTILIZATION_PCT of the window, reserving room
                // for this turn's max_tokens output — so we compact BEFORE the
                // provider would reject an over-long request, not after. Compute
                // on u64 with saturating_mul so an absurd configured value can't
                // wrap (real windows are millions, far from overflow).
                let cap = (self
                    .limits
                    .max_context_tokens
                    .saturating_mul(compress::COMPACT_UTILIZATION_PCT)
                    / 100) as usize;
                est + self.max_tokens as usize > cap
            } else {
                let threshold = self.limits.compress_threshold_chars;
                threshold > 0 && compress::total_chars(&self.messages) > threshold
            };
            if self.compress_failed || !over {
                return None;
            }
        } else if self.compress_failed {
            return None;
        }
        // Compression is best-effort: never let the summary request eat the
        // run's remaining budget — that would leave the main request with a
        // zero/near-zero timeout and fail the whole run with no result file.
        // Skip unless there's room for both the summary and the main request.
        if let Some(d) = deadline {
            if d.saturating_duration_since(Instant::now())
                < compress::COMPRESS_RESERVE + Duration::from_secs(5)
            {
                return None;
            }
        }
        let cut = compress::find_safe_cut(&self.messages, self.limits.keep_recent_messages)?;

        let (summary, sum_usage) = match self.summarize_prefix(cut, deadline) {
            Ok((s, u)) if !s.trim().is_empty() => (s, u),
            Ok((_empty, u)) => {
                // The summary request ran (so compression "happened") even
                // though the model returned empty/unusable text. Pass `u`
                // through unchanged: if the provider billed usage, the caller
                // counts/emits it; if it reported none, count nothing and emit
                // no noise Usage{0,0} event. Either way disable compression for
                // the rest of this run (don't retry every turn), and WARN — a
                // silent disable here (unlike the Err arm below) would leave a
                // growing context with no explanation for why it stops
                // compacting. Fires once: compress_failed gates future attempts.
                self.compress_failed = true;
                events(AgentEvent::Warning(
                    "context compression produced an empty summary (disabled for this run)".into(),
                ));
                return Some(u);
            }
            Err(e) => {
                self.compress_failed = true;
                events(AgentEvent::Warning(format!(
                    "context compression failed (disabled for this run): {e:#}"
                )));
                return None;
            }
        };

        let dropped = cut - 1;
        self.messages = compress::fold_summary(&self.messages, cut, &summary);
        if let Some(w) = self.save_session() {
            events(AgentEvent::Warning(w));
        }
        events(AgentEvent::Compressed {
            dropped,
            kept: self.messages.len(),
            // force=true is the overflow-recovery call (the provider rejected
            // the request as too long); the gated call is the preflight path.
            trigger: if force {
                CompressTrigger::Overflow
            } else {
                CompressTrigger::Preflight
            },
        });
        // The summary is a real billed request — pass its usage through so the
        // caller can add it to the totals. If the provider didn't report usage,
        // the inner is None and the caller counts/emits nothing (no Usage{0,0}
        // noise event in the NDJSON stream / TUI status).
        Some(sum_usage)
    }

    /// Build the transcript of the prefix being summarized and the prior running
    /// summary (if any). Pure function — unit-tested directly.
    #[cfg(test)]
    fn build_window_transcript(messages: &[Message], cut: usize) -> (String, Option<String>) {
        let old_summary = compress::extract_running_summary(messages);
        let transcript = compress::window_lines(messages, cut).concat();
        (transcript, old_summary)
    }

    /// Ask the LLM to summarize `messages[..cut]` into a compact running brief.
    /// Returns the summary text and the summed token usage across every summary
    /// call (single, or per-chunk + combine).
    ///
    /// Small/normal prefixes make ONE call with a proportional budget. A large
    /// prefix is split into message-aligned chunks (each ≤ SUMMARY_CHUNK_CHARS),
    /// each summarized at a fixed budget, then the chunk summaries (+ any prior
    /// running summary) are combined into the final brief. This bounds the
    /// per-call compression ratio (~6:1) so coding-relevant detail (paths,
    /// errors, decisions) survives, and keeps each call small enough that a
    /// summary request itself can't overflow — which matters for overflow
    /// recovery, which force-compresses exactly when the context is huge.
    fn summarize_prefix(
        &self,
        cut: usize,
        deadline: Option<Instant>,
    ) -> Result<(String, Option<Usage>)> {
        let old_summary = compress::extract_running_summary(&self.messages);
        let lines = compress::window_lines(&self.messages, cut);
        let transcript_size: usize = lines.iter().map(|l| l.len()).sum();

        if transcript_size <= compress::SUMMARY_CHUNK_CHARS {
            // Common case: one summary call with a proportional budget.
            let transcript = lines.concat();
            let budget = compress::summary_budget(transcript_size);
            self.summary_call(
                &compress::summary_prompt(&transcript, old_summary.as_deref()),
                budget,
                deadline,
                1,
            )
        } else {
            // Large prefix: summarize chunks, then combine.
            let chunks = compress::chunk_lines(lines, compress::SUMMARY_CHUNK_CHARS);
            // A single chunk (e.g. the transcript just cleared the limit but
            // packed into one piece) needs no separate combine round — summarize
            // it directly with the running-summary update prompt.
            if chunks.len() == 1 {
                return self.summary_call(
                    &compress::summary_prompt(&chunks[0], old_summary.as_deref()),
                    compress::summary_budget(chunks[0].len()),
                    deadline,
                    1,
                );
            }
            let n_calls = chunks.len() + 1; // per-chunk + combine
            let mut usage = None;
            let mut chunk_summaries = Vec::with_capacity(chunks.len());
            for chunk in &chunks {
                let (s, u) = self.summary_call(
                    &compress::summary_prompt(chunk, None),
                    compress::SUMMARY_CHUNK_BUDGET,
                    deadline,
                    n_calls,
                )?;
                usage = compress::merge_usage(usage, u);
                chunk_summaries.push(s);
            }
            // Combine the chunk summaries (+ prior running summary via the
            // prompt's old_summary slot) into the final updated brief.
            let mut combined = String::from(
                "Summaries of recent conversation segments, in chronological order:\n",
            );
            for (i, s) in chunk_summaries.iter().enumerate() {
                combined.push_str(&format!("\n[segment {}]\n{s}\n", i + 1));
            }
            let budget = compress::summary_budget(combined.len());
            let (final_summary, u) = self.summary_call(
                &compress::summary_prompt(&combined, old_summary.as_deref()),
                budget,
                deadline,
                n_calls,
            )?;
            usage = compress::merge_usage(usage, u);
            Ok((final_summary, usage))
        }
    }

    /// One summary chat call (no tools, no cache). `n_calls` shares the
    /// remaining run budget evenly across all summary calls of this compression
    /// so a multi-chunk summarize can't eat the main turn's time budget.
    fn summary_call(
        &self,
        prompt: &str,
        max_tokens: u32,
        deadline: Option<Instant>,
        n_calls: usize,
    ) -> Result<(String, Option<Usage>)> {
        let req = ChatRequest {
            system: Some(compress::SUMMARY_SYSTEM),
            messages: &[Message::user(vec![Block::Text(prompt.to_string())])],
            tools: &[],
            model: &self.model,
            max_tokens,
            // Even share of (remaining − reserve) across all summary calls, so
            // a multi-chunk compression can't starve the main chat that follows
            // in the same turn.
            per_request_timeout: deadline.map(|d| {
                let remaining = d
                    .saturating_duration_since(Instant::now())
                    .saturating_sub(compress::COMPRESS_RESERVE);
                remaining / (n_calls as u32)
            }),
            cancel: self.cancel_flag(),
            // The summary is one-off: the next compression summarizes a
            // different transcript, so a cache breakpoint would only ever pay
            // the 1.25× write premium with no subsequent read. Skip caching.
            cache: false,
        };
        let resp = self.provider.chat(&req, &mut |_| {})?;
        // parse_loop reports cancel/idle-timeout/truncation as Ok with a
        // non-end_turn stop_reason and PARTIAL, half-written blocks. Folding
        // such a summary into the history would poison every later turn, so
        // bail on those. max_tokens is DIFFERENT: the model stopped at the
        // output budget, so the text is coherent (just incomplete) — accepting
        // it beats disabling compression, because compress_failed also gates
        // the overflow-recovery path (two failures for the price of one).
        match resp.stop_reason.as_str() {
            "end_turn" | "max_tokens" => {}
            other => {
                anyhow::bail!("summary request did not complete (stop_reason={other})")
            }
        }
        let text = resp
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok((text, resp.usage))
    }
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;
