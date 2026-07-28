use crate::config::{Config, Limits};
use crate::provider::{Block, ChatEvent, ChatRequest, Message, Provider, Role, ToolDef, Usage};
use crate::tools::{Tool, ToolCtx, ToolOutput};
use anyhow::Result;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    /// and how many remain.
    Compressed { dropped: usize, kept: usize },
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
}

impl Agent {
    pub fn new(
        cfg: &Config,
        provider: Box<dyn Provider>,
        tools: Vec<Box<dyn Tool>>,
        initial: Vec<Message>,
        ctx: ToolCtx,
    ) -> Self {
        let tool_defs = tools.iter().map(|t| t.def()).collect();
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

    fn save_session(&self) {
        if let Some(path) = &self.session_path {
            if let Err(e) = crate::session::save(path, &self.model, &self.messages) {
                eprintln!("[warn] session save failed: {e:#}");
            }
        }
    }

    /// Append a user message (multi-turn / TUI follow-ups).
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub fn push_user(&mut self, blocks: Vec<Block>) {
        append_user_turn(&mut self.messages, blocks);
        self.save_session();
    }

    /// Drop the conversation transcript (`/clear`). Keeps config, tools, and
    /// system prompt — only messages are reset, and the session file is
    /// rewritten so a restart doesn't restore the cleared chat.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub fn clear_messages(&mut self) {
        self.messages.clear();
        self.save_session();
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
        // Running token totals for RunOutcome (the per-turn Usage event is also
        // emitted to the frontend; these accumulate for the result file).
        let mut in_tokens: u64 = 0;
        let mut out_tokens: u64 = 0;
        let mut cache_create: u64 = 0;
        let mut cache_read: u64 = 0;

        loop {
            if self.cancelled() {
                stop_reason = "cancelled".into();
                break;
            }
            if turns >= self.limits.max_turns {
                stop_reason = "max_turns_reached".into();
                break;
            }
            // timeout_secs == 0 means "no timeout" (mirroring the deadline
            // guard below). Without this check, elapsed() >= 0 is always true
            // and the agent would time out before its very first turn.
            if self.limits.timeout_secs > 0
                && start.elapsed().as_secs() >= self.limits.timeout_secs
            {
                stop_reason = "timeout".into();
                break;
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
                in_tokens += u.input_tokens;
                out_tokens += u.output_tokens;
                cache_create += u.cache_creation_input_tokens;
                cache_read += u.cache_read_input_tokens;
                events(AgentEvent::Usage {
                    input_tokens: u.input_tokens,
                    output_tokens: u.output_tokens,
                    cache_creation_input_tokens: u.cache_creation_input_tokens,
                    cache_read_input_tokens: u.cache_read_input_tokens,
                });
            }

            // Overflow recovery (OpenCode semantics): if the provider rejects
            // this request as too long for the context window, force ONE
            // compression and retry the turn on the shorter history; a second
            // overflow is a real error. The request borrows &self.messages, so
            // it lives in an inner block whose end releases that borrow before
            // the (mutable) compress call in the recovery arm.
            let mut overflow_recovered = false;
            let resp = loop {
                let before = self.messages.len();
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
                        // Headless has no flag → a dummy never-set one, so
                        // idle-timeout still applies.
                        cancel: self
                            .cancel
                            .clone()
                            .unwrap_or_else(|| Arc::new(AtomicBool::new(false))),
                        // The main turn benefits from prompt caching (re-reads
                        // the stable prefix across turns); one-off requests opt
                        // out.
                        cache: true,
                    };
                    self.provider.chat(&req, &mut |e| match e {
                        ChatEvent::TextDelta(t) => events(AgentEvent::TextDelta(t)),
                    })
                }; // req dropped here → &self.messages borrow released
                match r {
                    Ok(resp) => break resp,
                    Err(e) if !overflow_recovered && is_context_overflow(&e) => {
                        // Need a compressible head and a non-failed compressor;
                        // checked in the body (not the guard) to keep `self`
                        // borrows out of the match guard.
                        let can_compress = !self.compress_failed
                            && find_safe_cut(
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
                                in_tokens += u.input_tokens;
                                out_tokens += u.output_tokens;
                                cache_create += u.cache_creation_input_tokens;
                                cache_read += u.cache_read_input_tokens;
                                events(AgentEvent::Usage {
                                    input_tokens: u.input_tokens,
                                    output_tokens: u.output_tokens,
                                    cache_creation_input_tokens: u.cache_creation_input_tokens,
                                    cache_read_input_tokens: u.cache_read_input_tokens,
                                });
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
                in_tokens += u.input_tokens;
                out_tokens += u.output_tokens;
                cache_create += u.cache_creation_input_tokens;
                cache_read += u.cache_read_input_tokens;
                events(AgentEvent::Usage {
                    input_tokens: u.input_tokens,
                    output_tokens: u.output_tokens,
                    cache_creation_input_tokens: u.cache_creation_input_tokens,
                    cache_read_input_tokens: u.cache_read_input_tokens,
                });
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
            self.save_session();

            // A cancelled / idle-timeout / truncated / max_tokens stream may
            // leave a half-delivered tool_use whose input is partial or _raw.
            // OpenAI maps finish_reason "length" → "max_tokens"; when the
            // budget runs out mid tool-call arguments the truncated JSON fails
            // to parse and falls back to a _raw sentinel — executing that is
            // garbage. Anthropic is unaffected (it still emits content_block_stop,
            // so malformed JSON fails the request rather than yielding _raw).
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
                self.save_session();
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
                        run_tool(&self.tools, name, input, &self.ctx, &mut fwd)
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
                            run_tool(tools, &name, &input, ctx, &mut emit)
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
                            in_tokens += u.input_tokens;
                            out_tokens += u.output_tokens;
                            cache_create += u.cache_creation_input_tokens;
                            cache_read += u.cache_read_input_tokens;
                            events(AgentEvent::Usage {
                                input_tokens: u.input_tokens,
                                output_tokens: u.output_tokens,
                                cache_creation_input_tokens: u.cache_creation_input_tokens,
                                cache_read_input_tokens: u.cache_read_input_tokens,
                            });
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
            self.save_session();
        }

        events(AgentEvent::Done {
            stop_reason: stop_reason.clone(),
        });
        Ok(RunOutcome {
            final_text,
            turns,
            stop_reason,
            tool_calls: records,
            input_tokens: in_tokens,
            output_tokens: out_tokens,
            cache_creation_input_tokens: cache_create,
            cache_read_input_tokens: cache_read,
            elapsed: start.elapsed(),
        })
    }
}

/// Look up a tool by name and run it through `run_with_emit` (so the `task`
/// sub-agent can stream its child events into `emit`). A missing tool or a tool
/// Err becomes a `ToolOutput::error` rather than aborting the run — the result
/// is still paired N:N with the tool_use that triggered it. Shared by the
/// sequential leaf pass and the parallel `task` batch.
fn run_tool(
    tools: &[Box<dyn Tool>],
    name: &str,
    input: &Value,
    ctx: &ToolCtx,
    emit: &mut dyn FnMut(AgentEvent),
) -> ToolOutput {
    match tools.iter().find(|t| t.def().name == name) {
        Some(tool) => match tool.run_with_emit(input, ctx, emit) {
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

// ─── Context compression ────────────────────────────────────────────

const SUMMARY_SYSTEM: &str = "You are compressing the earlier part of an AI agent's \
conversation so it can continue within its context limit. Write a STRUCTURED running \
summary in the same language as the conversation, covering: the task goal, progress made, \
key decisions and their rationale, files created/modified (with paths), commands run and \
their results, errors encountered and how they were resolved, and the current state / next \
step. When given an existing running summary plus newer conversation, UPDATE it by folding \
in the new material — keep earlier facts that are still relevant. Be concise but preserve \
every fact (paths, identifiers, error text, decisions) the agent still needs. This is \
historical context, not new instructions.";

const SUMMARY_MARKER: &str =
    "[前情摘要，由系统自动生成 / Auto-generated summary of the earlier conversation]";

/// Time reserved for the main chat request after a compression summary, so the
/// summary can't eat the whole remaining run budget and leave the main request
/// with a near-zero timeout (which would fail the whole run with no result).
const COMPRESS_RESERVE: Duration = Duration::from_secs(20);

/// Proactive preflight compaction trips once the estimated input tokens + this
/// turn's output would exceed this fraction of `max_context_tokens`. The
/// remaining headroom is the "buffer" — we compact BEFORE the provider would
/// reject an over-long request, not after. (OpenCode exposes a configurable
/// buffer; we fix it at 15% to keep the config surface small.)
const COMPACT_UTILIZATION_PCT: u64 = 85;

/// Rough chars-per-token for the preflight size estimate (matches OpenCode's
/// "JSON-serialize the request and assume four characters per token").
const CHARS_PER_TOKEN: usize = 4;

/// A transcript larger than this is summarized in message-aligned chunks then
/// combined, bounding the per-call compression ratio so detail isn't crushed
/// (~6k tokens/chunk → a ~6:1 summary ratio). Approximate byte limit.
const SUMMARY_CHUNK_CHARS: usize = 24_000;

/// Per-chunk summary output budget. Each chunk is ≈6k tokens, so 1024 output
/// tokens keeps the chunk ratio near 6:1 (the same target as the proportional
/// single-call budget).
const SUMMARY_CHUNK_BUDGET: u32 = 1024;

/// Phrases a provider embeds in its error body when the request exceeds the
/// model's context window. Both providers surface the raw HTTP body in the
/// anyhow error's Display (`openai.rs` "…API error 400: {text}",
/// `anthropic.rs` "…API error {status}: {text}"), so a case-blind substring
/// match is enough. `body_fix_for_400` has already absorbed the max_tokens /
/// stream_options 400s, so only genuine overflow (and the odd 413) reaches
/// here. Conservative list covering OpenAI / Anthropic / DeepSeek / vLLM / Ollama.
const OVERFLOW_PHRASES: &[&str] = &[
    "context_length_exceeded",
    "maximum context length",
    "prompt is too long",
    "too many input tokens",
    "context length",
    "reduce the length",
    "reduce your prompt",
    "input length and max_tokens",
    "request_too_large",
];

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

/// Rough size estimate of a conversation (chars; images count cheaply).
fn total_chars(messages: &[Message]) -> usize {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .map(|b| match b {
            Block::Text(t) => t.len(),
            Block::Image { data, .. } => data.len() / 100, // base64 is ~33% overhead
            Block::ToolUse { input, .. } => input.to_string().len(),
            // A tool result may carry an image payload (e.g. load_image).
            // Count it like a top-level image, or a load_image-heavy
            // conversation never trips the compress threshold.
            Block::ToolResult { content, image, .. } => {
                let img = image
                    .as_deref()
                    .map(|b| match b {
                        Block::Image { data, .. } => data.len() / 100,
                        _ => 0,
                    })
                    .unwrap_or(0);
                content.len() + img
            }
        })
        .sum()
}

/// Rough token estimate of what the next request will cost the model: message
/// content + system prompt + advertised tool schemas, divided by the
/// chars-per-token heuristic. Used ONLY for the preflight "should I compact
/// before this request?" decision — never billed or reported; the provider's
/// real usage is the source of truth for accounting.
fn estimate_tokens(messages: &[Message], system: Option<&str>, tools: &[ToolDef]) -> usize {
    let tool_chars: usize = tools
        .iter()
        .map(|t| t.name.len() + t.description.len() + t.schema.to_string().len())
        .sum();
    let chars = total_chars(messages) + system.map(|s| s.len()).unwrap_or(0) + tool_chars;
    chars / CHARS_PER_TOKEN
}

/// True if a provider error looks like "the request was too long for the
/// model's context window" — i.e. compressing and retrying might let it fit.
/// See `OVERFLOW_PHRASES` for the match surface.
fn is_context_overflow(e: &anyhow::Error) -> bool {
    let msg = format!("{e:#}").to_lowercase();
    OVERFLOW_PHRASES.iter().any(|p| msg.contains(p))
}

/// Build the compressed history: original task (with the summary folded in
/// as one extra text block) + the untouched tail. Folding into message[0]
/// keeps proper user → assistant alternation when the tail starts with an
/// assistant message. Previous summary blocks are replaced, not stacked —
/// the new summary already absorbed them via the transcript.
fn fold_summary(messages: &[Message], cut: usize, summary: &str) -> Vec<Message> {
    let mut compressed = Vec::with_capacity(messages.len() - (cut - 1) + 1);
    let mut head = messages[0].clone();
    head.content
        .retain(|b| !matches!(b, Block::Text(t) if t.starts_with(SUMMARY_MARKER)));
    head.content
        .push(Block::Text(format!("{SUMMARY_MARKER}\n{summary}")));
    compressed.push(head);
    compressed.extend_from_slice(&messages[cut..]);
    compressed
}

/// Find a safe tail-start index. An assistant message is always a safe
/// boundary: its tool_use blocks (if any) are answered by the tool_result
/// that immediately follows it inside the kept tail. Cutting before a
/// tool_result-bearing user message would orphan those results, so those
/// are skipped. Must leave at least one middle message worth summarizing,
/// i.e. n must exceed keep_recent by at least 3 (the kept tail, the
/// assistant boundary to cut at, and one message before it to summarize).
fn find_safe_cut(messages: &[Message], keep_recent: usize) -> Option<usize> {
    let n = messages.len();
    if n <= keep_recent + 2 {
        return None;
    }
    let mut cut = n - keep_recent; // always >= 3 by the guard above
    while cut < n {
        if messages[cut].role == Role::Assistant {
            return Some(cut);
        }
        cut += 1;
    }
    None
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
                let est = estimate_tokens(&self.messages, self.system.as_deref(), &self.tool_defs);
                // Trip at COMPACT_UTILIZATION_PCT of the window, reserving room
                // for this turn's max_tokens output — so we compact BEFORE the
                // provider would reject an over-long request, not after.
                let cap = self.limits.max_context_tokens as usize
                    * COMPACT_UTILIZATION_PCT as usize
                    / 100;
                est + self.max_tokens as usize > cap
            } else {
                let threshold = self.limits.compress_threshold_chars;
                threshold > 0 && total_chars(&self.messages) > threshold
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
                < COMPRESS_RESERVE + Duration::from_secs(5)
            {
                return None;
            }
        }
        let cut = find_safe_cut(&self.messages, self.limits.keep_recent_messages)?;

        let (summary, sum_usage) = match self.summarize_prefix(cut, deadline) {
            Ok((s, u)) if !s.trim().is_empty() => (s, u),
            Ok((_empty, u)) => {
                // The summary request ran (so compression "happened") even
                // though the model returned empty/unusable text. Pass `u`
                // through unchanged: if the provider billed usage, the caller
                // counts/emits it; if it reported none, count nothing and emit
                // no noise Usage{0,0} event. Either way disable compression for
                // the rest of this run (don't retry every turn).
                self.compress_failed = true;
                return Some(u);
            }
            Err(e) => {
                self.compress_failed = true;
                eprintln!("[warn] context compression failed (disabled for this run): {e:#}");
                return None;
            }
        };

        let dropped = cut - 1;
        self.messages = fold_summary(&self.messages, cut, &summary);
        self.save_session();
        events(AgentEvent::Compressed {
            dropped,
            kept: self.messages.len(),
        });
        // The summary is a real billed request — pass its usage through so the
        // caller can add it to the totals. If the provider didn't report usage,
        // the inner is None and the caller counts/emits nothing (no Usage{0,0}
        // noise event in the NDJSON stream / TUI status).
        Some(sum_usage)
    }

    /// Pull the previous rolling summary out of the head message (if any). The
    /// summary is stored as a Text block beginning with SUMMARY_MARKER (see
    /// fold_summary); on a later compression we UPDATE it rather than re-feed
    /// it as raw transcript, so the model refines a running brief instead of
    /// re-summarizing its own output every time.
    fn extract_running_summary(messages: &[Message]) -> Option<String> {
        let head = messages.first()?;
        head.content.iter().find_map(|b| match b {
            Block::Text(t) if t.starts_with(SUMMARY_MARKER) => {
                // Strip the marker line; keep the summary body.
                Some(t[SUMMARY_MARKER.len()..].trim_start_matches('\n').to_string())
            }
            _ => None,
        })
    }

    /// Format each block of `messages[..cut]` (skipping the head's running-
    /// summary block) as a transcript line. Shared by the single-call and
    /// chunked summarize paths so the formatting lives in one place.
    fn window_lines(messages: &[Message], cut: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for (i, m) in messages[..cut].iter().enumerate() {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            for b in &m.content {
                // Skip the head's running-summary block — it's the thing we're
                // updating, not raw conversation to summarize.
                if i == 0 {
                    if let Block::Text(t) = b {
                        if t.starts_with(SUMMARY_MARKER) {
                            continue;
                        }
                    }
                }
                let line = match b {
                    Block::Text(t) => format!("[{role}] {t}\n"),
                    Block::Image { .. } => format!("[{role}] <image>\n"),
                    Block::ToolUse { name, input, .. } => {
                        format!("[{role}] tool_call {name}: {}\n", preview(&input.to_string()))
                    }
                    Block::ToolResult {
                        content, is_error, ..
                    } => {
                        let body = preview(content);
                        let tag = if *is_error { "tool_error" } else { "tool_result" };
                        format!("[{role}] {tag}: {body}\n")
                    }
                };
                lines.push(line);
            }
        }
        lines
    }

    /// Build the transcript of the prefix being summarized and the prior running
    /// summary (if any). Pure function — unit-tested directly.
    #[cfg(test)]
    fn build_window_transcript(messages: &[Message], cut: usize) -> (String, Option<String>) {
        let old_summary = Agent::extract_running_summary(messages);
        let transcript = Agent::window_lines(messages, cut).concat();
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
        let old_summary = Agent::extract_running_summary(&self.messages);
        let lines = Agent::window_lines(&self.messages, cut);
        let transcript_size: usize = lines.iter().map(|l| l.len()).sum();

        if transcript_size <= SUMMARY_CHUNK_CHARS {
            // Common case: one summary call with a proportional budget.
            let transcript = lines.concat();
            let budget = summary_budget(transcript_size);
            self.summary_call(
                &summary_prompt(&transcript, old_summary.as_deref()),
                budget,
                deadline,
                1,
            )
        } else {
            // Large prefix: summarize chunks, then combine.
            let chunks = chunk_lines(lines, SUMMARY_CHUNK_CHARS);
            let n_calls = chunks.len() + 1; // per-chunk + combine
            let mut usage = None;
            let mut chunk_summaries = Vec::with_capacity(chunks.len());
            for chunk in &chunks {
                let (s, u) = self.summary_call(
                    &summary_prompt(chunk, None),
                    SUMMARY_CHUNK_BUDGET,
                    deadline,
                    n_calls,
                )?;
                usage = merge_usage(usage, u);
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
            let budget = summary_budget(combined.len());
            let (final_summary, u) = self.summary_call(
                &summary_prompt(&combined, old_summary.as_deref()),
                budget,
                deadline,
                n_calls,
            )?;
            usage = merge_usage(usage, u);
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
            system: Some(SUMMARY_SYSTEM),
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
                    .saturating_sub(COMPRESS_RESERVE);
                remaining / (n_calls as u32)
            }),
            cancel: self
                .cancel
                .clone()
                .unwrap_or_else(|| Arc::new(AtomicBool::new(false))),
            // The summary is one-off: the next compression summarizes a
            // different transcript, so a cache breakpoint would only ever pay
            // the 1.25× write premium with no subsequent read. Skip caching.
            cache: false,
        };
        let resp = self.provider.chat(&req, &mut |_| {})?;
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

/// Proportional summary output budget: ≈16 bytes of transcript per output
/// token, clamped to [1024, 4096]. 100k bytes → 4096 tokens (~6:1), a small
/// window → 1024. Replaces the old fixed 2048, which crushed a 100k-char
/// prefix at ~12:1 and dropped coding-relevant detail.
fn summary_budget(size: usize) -> u32 {
    ((size / 16) as u32).clamp(1024, 4096)
}

/// Build the user message for a summary call. With an `old_summary` we ask the
/// model to UPDATE a running brief from new material; otherwise to summarize
/// fresh. The system prompt (SUMMARY_SYSTEM) is shared.
fn summary_prompt(body: &str, old_summary: Option<&str>) -> String {
    match old_summary {
        Some(old) if !old.trim().is_empty() => format!(
            "Below is the current running summary of the earlier conversation, followed by newer \
             conversation. Produce an UPDATED running summary that folds in the new material while \
             keeping earlier facts that are still relevant.\n\n\
             ## Current running summary\n{old}\n\n## Newer conversation\n{body}"
        ),
        _ => format!("Summarize this conversation transcript:\n\n{body}"),
    }
}

/// Split transcript lines into chunks each ≤ `max_bytes`, breaking only at line
/// boundaries (so a tool_use/tool_result block stays in one chunk). A single
/// oversized line becomes its own chunk rather than being split mid-block.
fn chunk_lines(lines: Vec<String>, max_bytes: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for line in lines {
        if !cur.is_empty() && cur.len() + line.len() > max_bytes {
            chunks.push(std::mem::take(&mut cur));
        }
        cur.push_str(&line);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// Add two optional per-request usages into one. Either side may be `None`
/// (provider reported no usage); the result carries whichever was billed.
fn merge_usage(a: Option<Usage>, b: Option<Usage>) -> Option<Usage> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x),
        (Some(mut x), Some(y)) => {
            x.input_tokens += y.input_tokens;
            x.output_tokens += y.output_tokens;
            x.cache_creation_input_tokens += y.cache_creation_input_tokens;
            x.cache_read_input_tokens += y.cache_read_input_tokens;
            Some(x)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::mock::{self, MockProvider, MockTurn};
    use crate::provider::ChatResponse;
    use crate::tools::ToolOutput;
    use serde_json::json;

    fn user_text(s: &str) -> Message {
        Message::user(vec![Block::Text(s.into())])
    }

    fn tool_turn(id: &str) -> Vec<Message> {
        vec![
            Message::assistant(vec![Block::ToolUse {
                id: id.into(),
                name: "shell".into(),
                input: json!({"command": "ls"}),
            }]),
            Message::user(vec![Block::ToolResult {
                tool_use_id: id.into(),
                content: "ok".into(),
                image: None,
                is_error: false,
            }]),
        ]
    }

    #[test]
    fn safe_cut_skips_tool_pairs() {
        // [task][user a][tool pair][user b][tool pair at tail]
        let mut msgs = vec![user_text("task"), user_text("a")];
        msgs.extend(tool_turn("t1"));
        msgs.push(user_text("b"));
        msgs.extend(tool_turn("t2"));
        // indices: 0 task, 1 a, 2 asst-tu, 3 user-tr, 4 b, 5 asst-tu, 6 user-tr
        // keep_recent=2 → cut at 5 (assistant w/ tool_use); its tool_result
        // at index 6 stays in the tail — pair intact.
        assert_eq!(find_safe_cut(&msgs, 2), Some(5));
        // keep_recent=4 → candidate 3 (tool_result user) → slides past 4
        // (plain user) to 5 (assistant).
        assert_eq!(find_safe_cut(&msgs, 4), Some(5));
    }

    #[test]
    fn safe_cut_slides_past_tool_results() {
        // Pure tool loop: [task][asst tu][user tr] × 4
        let mut msgs = vec![user_text("task")];
        for i in 0..4 {
            msgs.extend(tool_turn(&format!("t{i}")));
        }
        // indices: 0 task, 1 asst, 2 user-tr, 3 asst, 4 user-tr, ...
        // keep_recent=2 → candidate cut at 7 = assistant (pair t3 intact).
        assert_eq!(find_safe_cut(&msgs, 2), Some(7));
        // keep_recent=6 → candidate cut at 3 = assistant.
        assert_eq!(find_safe_cut(&msgs, 6), Some(3));
    }

    #[test]
    fn append_user_turn_merges_into_trailing_user() {
        // History ends in a user message (e.g. the prior turn was cancelled
        // before any assistant text, so its assistant message was dropped).
        // The follow-up must merge, not append a second user message — two
        // adjacent user messages are a 400.
        let mut msgs = vec![user_text("task")];
        append_user_turn(&mut msgs, vec![Block::Text("again".into())]);
        assert_eq!(msgs.len(), 1, "no second user message");
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[0].content.len(), 2, "blocks merged into the one turn");
    }

    #[test]
    fn append_user_turn_pushes_new_after_assistant() {
        // Normal turn boundary: assistant spoke, now a user follow-up starts a
        // new message (no trailing user to merge into).
        let mut msgs = vec![
            user_text("task"),
            Message::assistant(vec![Block::Text("hi".into())]),
        ];
        append_user_turn(&mut msgs, vec![Block::Text("more".into())]);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2].role, Role::User);
    }

    #[test]
    fn safe_cut_too_short_history() {
        let msgs = vec![user_text("task"), user_text("hello"), user_text("more")];
        assert_eq!(find_safe_cut(&msgs, 8), None); // n <= keep_recent + 2
    }

    #[test]
    fn safe_cut_needs_middle_content() {
        // Candidate lands at index 1 (assistant) → nothing to summarize → None
        let msgs = vec![
            user_text("task"),
            Message::assistant(vec![Block::Text("r1".into())]),
            Message::assistant(vec![Block::Text("r2".into())]),
            Message::assistant(vec![Block::Text("r3".into())]),
            Message::assistant(vec![Block::Text("r4".into())]),
        ];
        assert_eq!(find_safe_cut(&msgs, 3), None);
    }

    #[test]
    fn fold_summary_replaces_old_summary_keeps_tail() {
        // Simulate a second compression: head already carries a summary
        // block; folding a new one must replace it, not stack.
        let mut msgs = vec![user_text("task"), user_text("old turn")];
        msgs = fold_summary(&msgs, 2, "summary-one");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content.len(), 2); // task + summary

        msgs.extend(tool_turn("t9"));
        let cut = 1; // drop everything between head and the tail
        let out = fold_summary(&msgs, cut, "summary-two");
        // head + 2 tail messages
        assert_eq!(out.len(), 3);
        // head: task + exactly one summary block, and it's the new one
        let summaries: Vec<_> = out[0]
            .content
            .iter()
            .filter(|b| matches!(b, Block::Text(t) if t.starts_with(SUMMARY_MARKER)))
            .collect();
        assert_eq!(summaries.len(), 1);
        assert!(matches!(summaries[0], Block::Text(t) if t.contains("summary-two")));
        assert!(matches!(&out[0].content[0], Block::Text(t) if t == "task"));
        // tail untouched
        assert_eq!(out[1].role, Role::Assistant);
        assert_eq!(out[2].role, Role::User);
    }

    #[test]
    fn total_chars_counts_blocks() {
        let msgs = vec![
            user_text("12345"),
            Message::assistant(vec![Block::ToolUse {
                id: "x".into(),
                name: "s".into(),
                input: json!({"a": 1}),
            }]),
        ];
        // "12345" (5) + "{\"a\":1}" (7)
        assert_eq!(total_chars(&msgs), 12);
    }

    #[test]
    fn total_chars_counts_tool_result_image() {
        // A load_image result carries its image inside ToolResult.image;
        // that payload must count toward the compress threshold just like
        // a top-level Block::Image, or image-heavy chats never compress.
        let msgs = vec![Message::user(vec![
            Block::Text("note".into()), // 4
            Block::ToolResult {
                tool_use_id: "x".into(),
                content: "see image".into(), // 9
                image: Some(Box::new(Block::Image {
                    media_type: "image/png".into(),
                    data: "A".repeat(1000), // 1000 / 100 = 10
                })),
                is_error: false,
            },
        ])];
        // 4 + 9 + 10 = 23
        assert_eq!(total_chars(&msgs), 23);
    }

    #[test]
    fn preview_truncates_in_one_pass() {
        // Single-pass truncation: up to 200 chars kept verbatim, 201+ gets an
        // ellipsis; the exact boundary (MAX) must NOT add one.
        assert_eq!(preview("hi"), "hi");
        let exact: String = "b".repeat(200);
        let p = preview(&exact);
        assert_eq!(p.chars().count(), 200);
        assert!(!p.ends_with('…'));
        let over = preview(&"a".repeat(500));
        assert!(over.ends_with('…'));
        assert_eq!(over.chars().count(), 201); // 200 + ellipsis
    }

    // ── agent-loop invariants (driven by the in-process MockProvider) ─────
    //
    // These pin down the structural rules the loop relies on — the ones that
    // used to be covered only by hand-run Python mocks. Each names the
    // invariant it guards so a future change that breaks it fails loudly.

    /// A minimal Config built from JSON (no file, no API-key lookup — the
    /// agent never calls `api_key()`; the test wires a MockProvider directly).
    fn cfg_with(max_turns: u32, timeout_secs: u64, compress: usize) -> Config {
        let mut c = serde_json::from_str::<Config>(
            r#"{"provider":"openai","api_key_env":"MOCK_API_KEY","model":"m"}"#,
        )
        .expect("valid test config");
        c.limits.max_turns = max_turns;
        c.limits.timeout_secs = timeout_secs;
        c.limits.compress_threshold_chars = compress;
        c
    }

    fn test_ctx() -> ToolCtx {
        ToolCtx {
            workdir: std::env::current_dir().expect("cwd"),
            max_output_chars: 1000,
            restrict_to_workdir: false,
            cancel: None,
        }
    }

    /// A no-op tool that can also (a) record that it ran, and (b) flip a
    /// shared cancel flag mid-run — to exercise the batch-cancel path.
    struct EchoTool {
        name: String,
        ran: Option<Arc<AtomicBool>>,
        cancel_on_run: Option<Arc<AtomicBool>>,
    }
    impl EchoTool {
        fn new(name: &str) -> Self {
            Self { name: name.into(), ran: None, cancel_on_run: None }
        }
        fn tracking(name: &str, ran: Arc<AtomicBool>) -> Self {
            Self { name: name.into(), ran: Some(ran), cancel_on_run: None }
        }
        fn cancel_on_run(name: &str, flag: Arc<AtomicBool>) -> Self {
            Self { name: name.into(), ran: None, cancel_on_run: Some(flag) }
        }
    }
    impl Tool for EchoTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: self.name.clone(),
                description: "test echo tool".into(),
                schema: json!({"type": "object", "properties": {}}),
            }
        }
        fn run(&self, _input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
            if let Some(r) = &self.ran {
                r.store(true, Ordering::Relaxed);
            }
            if let Some(c) = &self.cancel_on_run {
                c.store(true, Ordering::Relaxed);
            }
            Ok(ToolOutput::text("ok"))
        }
    }

    /// A tool that bills synthetic usage to the parent run — pins that
    /// `ToolOutput::usage` folds into the agent's totals and emits a Usage
    /// event. This is the path the `task` sub-agent relies on for honest cost.
    struct BillTool;
    impl Tool for BillTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: "bill".into(),
                description: "test usage-billing tool".into(),
                schema: json!({"type": "object", "properties": {}}),
            }
        }
        fn run(&self, _input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
            Ok(ToolOutput {
                content: "sub-work done".into(),
                image: None,
                is_error: false,
                usage: Some(crate::provider::Usage {
                    input_tokens: 111,
                    output_tokens: 22,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                }),
            })
        }
    }

    /// A tool that streams synthetic sub-agent events into the parent run via
    /// `run_with_emit` — pins that the tool loop routes the `emit` closure
    /// through to the parent's event sink (the whole point of child-event
    /// visibility for the `task` tool). Mirrors how `SubAgent` forwards its
    /// child's events wrapped as `AgentEvent::SubAgent{..}`.
    struct EmitTool;
    impl Tool for EmitTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: "emit".into(),
                description: "test tool that forwards sub-agent events".into(),
                schema: json!({"type": "object", "properties": {}}),
            }
        }
        fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
            self.run_with_emit(input, ctx, &mut |_| {})
        }
        fn run_with_emit(
            &self,
            _input: &Value,
            _ctx: &ToolCtx,
            emit: &mut dyn FnMut(AgentEvent),
        ) -> Result<ToolOutput> {
            emit(AgentEvent::SubAgent {
                name: "scout".into(),
                event: Box::new(AgentEvent::TextDelta("looking".into())),
            });
            emit(AgentEvent::SubAgent {
                name: "scout".into(),
                event: Box::new(AgentEvent::TextDelta(" found".into())),
            });
            Ok(ToolOutput::text("scout done"))
        }
    }

    /// A stand-in for the real `task` sub-agent tool, named "task" so the loop's
    /// partition routes it to the PARALLEL batch (the whole point of commit 2).
    /// It reads `input["label"]` to make each call's output distinguishable, emits
    /// a name-tagged SubAgent event (so we can assert it ran), and bills usage
    /// (so we can assert parallel cost folds into the run totals). Concurrency is
    /// structural — we assert functional correctness, NOT timing (which would flake).
    struct TaskTool;
    impl Tool for TaskTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: "task".into(),
                description: "test stand-in for the task sub-agent tool".into(),
                schema: json!({"type": "object", "properties": {"label": {"type": "string"}}}),
            }
        }
        fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
            self.run_with_emit(input, ctx, &mut |_| {})
        }
        fn run_with_emit(
            &self,
            input: &Value,
            _ctx: &ToolCtx,
            emit: &mut dyn FnMut(AgentEvent),
        ) -> Result<ToolOutput> {
            let label = input["label"].as_str().unwrap_or("?").to_string();
            emit(AgentEvent::SubAgent {
                name: label.clone(),
                event: Box::new(AgentEvent::TextDelta(format!("work-{label}"))),
            });
            Ok(ToolOutput {
                content: format!("out-{label}"),
                image: None,
                is_error: false,
                usage: Some(crate::provider::Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                }),
            })
        }
    }

    #[test]
    fn tool_reported_usage_folds_into_run_totals_and_events() {
        // A tool reporting usage (the `task` sub-agent path) must fold into the
        // run's token totals AND emit its own AgentEvent::Usage, alongside each
        // turn's streamed usage. Pins the honest-cost fold the sub-agent relies
        // on — touched the core tool-result path, so this stays green.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("t", "bill", json!({}), mock::mk_usage(50, 7)),
            // Turn 2 is the sole cache contributor, so cache totals are unambiguous.
            mock::resp_text("final", mock::mk_usage_cache(0, 0, 5, 9)),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(BillTool)],
            vec![user_text("go")],
            test_ctx(),
        );
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        // Streamed turn usage (50↑/7↓, then 0/0) + the tool's billed (111↑/22↓).
        assert_eq!(o.input_tokens, 50 + 111);
        assert_eq!(o.output_tokens, 7 + 22);
        // Cache only from turn 2 (5 write / 9 read); the tool billed none.
        assert_eq!(o.cache_creation_input_tokens, 5);
        assert_eq!(o.cache_read_input_tokens, 9);
        // The tool's billed usage surfaced as its own Usage event (111/22).
        let usage_events: Vec<(u64, u64)> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Usage { input_tokens, output_tokens, .. } => Some((*input_tokens, *output_tokens)),
                _ => None,
            })
            .collect();
        assert!(
            usage_events.contains(&(111, 22)),
            "tool-reported usage must emit a Usage event; got {usage_events:?}"
        );
    }

    #[test]
    fn tool_emitted_subagent_events_reach_parent_stream() {
        // A tool overriding run_with_emit (the `task` path) forwards its
        // AgentEvent::SubAgent{..} into the parent's run callback, tagged with
        // the sub-agent's name — that identity is what disambiguates parallel
        // sub-agents. Pins the loop's routing of the `emit` closure, which the
        // sequential v1 changed from a plain `tool.run(...)` call.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("t", "emit", json!({}), mock::mk_usage(1, 1)),
            mock::resp_text("final", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EmitTool)],
            vec![user_text("go")],
            test_ctx(),
        );
        let mut events = Vec::new();
        agent.run(&mut |e| events.push(e)).unwrap();
        // Collect forwarded sub-agent text deltas, grouped by name.
        let forwarded: Vec<(String, String)> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::SubAgent { name, event } => match &**event {
                    AgentEvent::TextDelta(t) => Some((name.clone(), t.clone())),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert!(
            forwarded.iter().all(|(n, _)| n == "scout"),
            "every forwarded event must be tagged scout: {forwarded:?}"
        );
        let text: String = forwarded.into_iter().map(|(_, t)| t).collect();
        assert_eq!(text, "looking found", "both deltas forwarded in order");
    }

    #[test]
    fn parallel_task_calls_both_run_and_keep_order_with_usage_folded() {
        // One turn with TWO `task` calls (labels a/b). The parallel batch must
        // run BOTH (assert via the forwarded SubAgent events), keep results in
        // the ORIGINAL block order despite concurrent execution (N:N + ordered
        // records), and fold each task's billed usage into the run totals.
        // We assert functional correctness only — concurrency is structural
        // (thread::scope), never timing.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![MockTurn::Ok(ChatResponse {
            blocks: vec![
                Block::ToolUse {
                    id: "a".into(),
                    name: "task".into(),
                    input: json!({"label": "a"}),
                },
                Block::ToolUse {
                    id: "b".into(),
                    name: "task".into(),
                    input: json!({"label": "b"}),
                },
            ],
            stop_reason: "tool_use".into(),
            usage: None,
        }),
        // Turn 2 reports no usage so the only input tokens are the two tasks'.
        mock::resp_text("final", None)]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(TaskTool)],
            vec![user_text("go")],
            test_ctx(),
        );
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();

        // Both tasks ran — each forwarded a SubAgent event tagged with its label.
        let tags: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::SubAgent { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert!(tags.contains(&"a".into()) && tags.contains(&"b".into()), "both ran: {tags:?}");

        // Both tasks' ToolCall events still fire (now emitted from the batch
        // block after the gate, not the inline pass) — in block order a, b.
        let toolcalls: Vec<&Value> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolCall { name, input } if name == "task" => Some(input),
                _ => None,
            })
            .collect();
        assert_eq!(toolcalls.len(), 2, "both task ToolCalls must fire: {toolcalls:?}");
        assert_eq!(toolcalls[0]["label"], "a");
        assert_eq!(toolcalls[1]["label"], "b");

        // Results assembled in ORIGINAL block order (a before b), not completion
        // order — records must stay coherent with the tool_use sequence.
        assert_eq!(o.tool_calls.len(), 2);
        assert_eq!(o.tool_calls[0].output, "out-a");
        assert_eq!(o.tool_calls[1].output, "out-b");
        // (N:N result pairing + synthesis is pinned by the cancel test below,
        // where the run stops right after the batch so the trailing message is
        // the tool_results; here turn 2 produces the assistant's final text.)

        // Each task billed 100 input tokens → 200 total (turn usage was None/0).
        assert_eq!(o.input_tokens, 200);
        assert_eq!(o.output_tokens, 20);
    }

    #[test]
    fn cancel_gates_the_parallel_task_batch_and_keeps_n_n_pairing() {
        // A leaf tool ("trip") flips cancel mid-batch; the FOLLOWING `task` call
        // must be gated off (never spawned) — the batch checks `batch_cancelled`
        // before fanning out. Yet the orphaned task_use still gets a synthesized
        // "[cancelled by user]" result so pairing holds (N:N), and it is NOT
        // recorded as executed.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![MockTurn::Ok(ChatResponse {
            blocks: vec![
                Block::ToolUse { id: "a".into(), name: "trip".into(), input: json!({}) },
                Block::ToolUse { id: "b".into(), name: "task".into(), input: json!({"label": "x"}) },
            ],
            stop_reason: "tool_use".into(),
            usage: None,
        })]);
        let flag = Arc::new(AtomicBool::new(false));
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![
                Box::new(EchoTool::cancel_on_run("trip", flag.clone())),
                Box::new(TaskTool),
            ],
            vec![user_text("go")],
            test_ctx(),
        );
        agent.set_cancel(flag);
        let mut saw_task_subagent = false;
        let mut saw_task_toolcall = false;
        let o = agent.run(&mut |e| match &e {
            AgentEvent::SubAgent { name, .. } if name == "x" => saw_task_subagent = true,
            AgentEvent::ToolCall { name, .. } if name == "task" => saw_task_toolcall = true,
            _ => {}
        })
        .unwrap();

        assert_eq!(o.stop_reason, "cancelled");
        // Only the leaf "trip" executed; the gated `task` is NOT recorded.
        assert_eq!(o.tool_calls.len(), 1);
        assert_eq!(o.tool_calls[0].name, "trip");
        // The task never ran (no forwarded SubAgent event for label "x").
        assert!(!saw_task_subagent, "gated task must not have spawned");
        // And it never had its ToolCall announced either — emitting it before the
        // gate would dangle a ToolCall with no matching ToolResult on the UI.
        assert!(
            !saw_task_toolcall,
            "gated task must not emit a ToolCall (would dangle without a ToolResult)"
        );
        // N:N pairing intact: trip → "ok", task → synthesized cancelled result.
        assert_eq!(
            agent.trailing_tool_result_contents(),
            vec!["ok".to_string(), "[cancelled by user]".to_string()]
        );
    }

    #[test]
    fn run_resets_stale_cancel_so_first_turn_is_not_cancelled() {
        // A cancel left set from a previous run must be cleared at entry —
        // cancel is level-triggered and never auto-clears elsewhere, so
        // without the reset the first turn breaks "cancelled" before chat.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![mock::resp_text("done", None)]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("hi")], test_ctx());
        agent.set_cancel(Arc::new(AtomicBool::new(true))); // stale cancel
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(o.turns, 1);
    }

    #[test]
    fn run_as_subagent_does_not_reset_a_set_cancel_flag() {
        // The `task` sub-agent's child shares the parent's cancel Arc and runs
        // via run_as_subagent, which — unlike the top-level run — must NOT reset
        // the flag at entry. If it did, a sibling starting later in a parallel
        // `task` batch would clear a Ctrl-C another child (or the parent) is
        // mid-responding to, dropping the cancel for the whole batch (the race
        // CLAUDE.md records batch::run avoiding by not sharing the flag). Pin
        // the invariant directly: with the flag already set, the sub-agent run
        // honours it (stop_reason "cancelled") and leaves the flag set, rather
        // than resetting it and proceeding.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![mock::resp_text("x", None)]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("hi")], test_ctx());
        let flag = Arc::new(AtomicBool::new(true)); // already cancelled
        agent.set_cancel(flag.clone());
        let o = agent.run_as_subagent(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "cancelled", "sub-agent must honour a set flag, not reset it");
        assert!(
            flag.load(Ordering::Relaxed),
            "entry must not have cleared the shared flag"
        );
    }

    /// A Provider that flips a shared cancel flag as `chat()` returns, with a
    /// stop_reason NOT in the abort set ("end_turn"). Used to exercise the M8
    /// window: a Ctrl-C lands between `chat()` returning and the abort check,
    /// so the provider's own stop_reason is still "end_turn" — the loop must
    /// override it to "cancelled", or the run exits 0 (end_turn) instead of 130.
    struct CancelOnChatProvider {
        flag: Arc<AtomicBool>,
    }
    impl Provider for CancelOnChatProvider {
        fn chat(&self, _req: &ChatRequest, on_event: &mut dyn FnMut(ChatEvent)) -> Result<ChatResponse> {
            on_event(ChatEvent::TextDelta("partial".into()));
            // Simulate a Ctrl-C landing as the stream wraps up: the agent's own
            // cancel Arc flips true here — AFTER run()'s entry reset, BEFORE the
            // abort check runs. (A real cancel during the final streaming bytes
            // hits the same window: CancellableSse breaks the stream, chat()
            // returns with whatever stop_reason the provider last sent.)
            self.flag.store(true, Ordering::Relaxed);
            Ok(ChatResponse {
                blocks: vec![Block::Text("partial".into())],
                stop_reason: "end_turn".into(),
                usage: None,
            })
        }
    }

    #[test]
    fn cancel_arriving_after_chat_reports_cancelled_not_end_turn() {
        // M8: a Ctrl-C that lands in the window between chat() returning and the
        // abort check must surface stop_reason "cancelled" (→ exit 130), even
        // though the provider's own stop_reason was "end_turn". Without the
        // override the run reports "end_turn" (→ exit 0) and silently swallows
        // the cancel — a real Ctrl-C during a response's final streaming bytes
        // used to exit 0 instead of 130.
        let cfg = cfg_with(5, 0, 0);
        let flag = Arc::new(AtomicBool::new(false));
        let prov = CancelOnChatProvider { flag: flag.clone() };
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("hi")], test_ctx());
        agent.set_cancel(flag);
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "cancelled");
        // The partial text the provider streamed still surfaces to the caller.
        assert_eq!(o.final_text, "partial");
    }

    #[test]
    fn empty_assistant_turn_is_not_persisted() {
        // M10: a content-filtered / genuinely empty response (zero blocks,
        // stop_reason "end_turn") must NOT be pushed as an empty assistant
        // message — Anthropic requires each message to carry >=1 content block,
        // so an empty assistant turn makes EVERY later request 400 until the
        // session is cleared. The loop stops here without persisting the turn.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![MockTurn::Ok(ChatResponse {
            blocks: vec![],
            stop_reason: "end_turn".into(),
            usage: None,
        })]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("hi")], test_ctx());
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(o.turns, 1);
        assert_eq!(o.final_text, "");
        // No empty assistant message was pushed: history is still just the one
        // user turn (same-module access to the private `messages` field).
        assert_eq!(
            agent.messages.len(),
            1,
            "empty assistant turn must not be persisted"
        );
        assert_eq!(agent.messages[0].role, Role::User);
    }

    #[test]
    fn parallel_tool_uses_pair_with_n_tool_results() {
        // One assistant turn with 2 tool_use → the NEXT request's trailing
        // message must be a user turn carrying 2 tool_results (N:N), or a
        // strict provider rejects the orphaned tool_use with a 400.
        let cfg = cfg_with(5, 0, 0);
        let (prov, state) = MockProvider::new(vec![
            MockTurn::Ok(ChatResponse {
                blocks: vec![
                    Block::ToolUse { id: "a".into(), name: "echo".into(), input: json!({}) },
                    Block::ToolUse { id: "b".into(), name: "echo".into(), input: json!({}) },
                ],
                stop_reason: "tool_use".into(),
                usage: None,
            }),
            mock::resp_text("all done", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("echo"))],
            vec![user_text("do it")],
            test_ctx(),
        );
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(o.tool_calls.len(), 2);
        // The 2nd chat saw a trailing USER message with exactly 2 tool_results.
        let cap = state.captured();
        assert_eq!(cap.len(), 2);
        assert_eq!(cap[1].last_role, Some(Role::User));
        assert_eq!(cap[1].trailing_tool_results, 2);
    }

    #[test]
    fn cancel_mid_batch_synthesizes_results_for_remaining_tools() {
        // 2 parallel tool calls; the first tool flips cancel. The second must
        // STILL get a tool_result ("[cancelled by user]") so the assistant's
        // 2 tool_use are answered 1:1, and the run ends "cancelled".
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![MockTurn::Ok(ChatResponse {
            blocks: vec![
                Block::ToolUse { id: "a".into(), name: "echo".into(), input: json!({}) },
                Block::ToolUse { id: "b".into(), name: "echo".into(), input: json!({}) },
            ],
            stop_reason: "tool_use".into(),
            usage: None,
        })]);
        let flag = Arc::new(AtomicBool::new(false));
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::cancel_on_run("echo", flag.clone()))],
            vec![user_text("go")],
            test_ctx(),
        );
        agent.set_cancel(flag); // so cancelled() is observable; the tool flips it
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "cancelled");
        // Only the first tool actually executed; the cancelled one is NOT in
        // tool_calls (those record real executions).
        assert_eq!(o.tool_calls.len(), 1);
        assert!(!o.tool_calls[0].is_error);
        // But the message history still answers both tool_use 1:1 — the
        // cancelled tool gets a synthesized result so pairing holds.
        assert_eq!(
            agent.trailing_tool_result_contents(),
            vec!["ok".to_string(), "[cancelled by user]".to_string()]
        );
    }

    #[test]
    fn aborted_stream_does_not_execute_truncated_tool_use() {
        // stop_reason "max_tokens" with a half-delivered tool_use: the tool
        // must NOT run (executing truncated args is garbage), and no
        // tool_result is synthesized — the orphaned tool_use is dropped.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![MockTurn::Ok(ChatResponse {
            blocks: vec![
                Block::Text("partial...".into()),
                Block::ToolUse { id: "x".into(), name: "echo".into(), input: json!({"_raw": "trun"}) },
            ],
            stop_reason: "max_tokens".into(),
            usage: None,
        })]);
        let ran = Arc::new(AtomicBool::new(false));
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::tracking("echo", ran.clone()))],
            vec![user_text("go")],
            test_ctx(),
        );
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "max_tokens");
        assert!(o.tool_calls.is_empty(), "truncated tool_use must not execute");
        assert!(!ran.load(Ordering::Relaxed), "tool.run was not called");
        assert_eq!(o.final_text, "partial..."); // partial text still surfaced
    }

    #[test]
    fn max_turns_caps_the_loop() {
        // max_turns=1: exactly one chat call, then stop — even though the
        // model keeps requesting tools.
        let cfg = cfg_with(1, 0, 0);
        let (prov, state) = MockProvider::new(vec![
            mock::resp_tool("a", "echo", json!({}), None),
            mock::resp_tool("b", "echo", json!({}), None), // turn 2, never reached
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("echo"))],
            vec![user_text("loop")],
            test_ctx(),
        );
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "max_turns_reached");
        assert_eq!(o.turns, 1);
        assert_eq!(state.calls(), 1, "only one chat call within the budget");
    }

    #[test]
    fn timeout_zero_means_no_timeout() {
        // Guards the elapsed()>=0 trap: with timeout_secs=0 the run must
        // proceed normally instead of timing out before its first turn.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![mock::resp_text("ok", None)]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("hi")], test_ctx());
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(o.turns, 1);
    }

    #[test]
    fn usage_accumulates_across_turns_and_emits_events() {
        // Token usage from every chat sums into RunOutcome and is also emitted
        // per-turn (so the TUI bar / result file report honest totals).
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("a", "echo", json!({}), mock::mk_usage(100, 10)),
            mock::resp_text("done", mock::mk_usage(50, 5)),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("echo"))],
            vec![user_text("go")],
            test_ctx(),
        );
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.input_tokens, 150);
        assert_eq!(o.output_tokens, 15);
        let usage_n = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Usage { .. }))
            .count();
        assert_eq!(usage_n, 2);
    }

    #[test]
    fn cache_usage_propagates_to_run_outcome_and_event() {
        // Prompt-cache tokens billed by the provider (Anthropic cache writes/
        // reads) must flow through to the RunOutcome totals AND the per-turn
        // Usage event — otherwise caching-on would under-report real cost.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("a", "echo", json!({}), mock::mk_usage_cache(100, 10, 300, 1500)),
            mock::resp_text("done", mock::mk_usage_cache(50, 5, 0, 1200)),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("echo"))],
            vec![user_text("go")],
            test_ctx(),
        );
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        // input/output accumulate as before...
        assert_eq!(o.input_tokens, 150);
        assert_eq!(o.output_tokens, 15);
        // ...and so do the cache fields: 300+0 writes, 1500+1200 reads.
        assert_eq!(o.cache_creation_input_tokens, 300);
        assert_eq!(o.cache_read_input_tokens, 2700);
        // The per-turn Usage event carries the cache fields too.
        let cache_events = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    AgentEvent::Usage {
                        cache_creation_input_tokens: _,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(cache_events, 2);
    }

    #[test]
    fn compress_folds_history_and_emits_event() {
        // History over threshold + a safe cut → summarize runs (an extra chat
        // call), the middle folds into message[0], a Compressed event fires,
        // and the main request sees the shorter folded history.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        // 5 messages → n > keep_recent+2; cut candidate 3 = assistant → Some(3)
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", None),     // summarize_prefix call
            mock::resp_text("final answer", None), // main call
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(state.calls(), 2);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Compressed { dropped: 2, kept: 3 })),
            "compress should drop 2 middle messages, keep 3"
        );
        // The main (2nd) request saw the folded history: 3 messages.
        let cap = state.captured();
        assert_eq!(cap[1].message_count, 3);
    }

    #[test]
    fn compress_preserves_tool_use_tool_result_pairing_in_tail() {
        // Integration-level companion to `safe_cut_skips_tool_pairs` (which
        // only unit-tests find_safe_cut's cut SELECTION). This runs a REAL
        // compression — summarize_prefix + fold_summary through
        // compress_if_needed — with a tool_use ↔ tool_result pair sitting at
        // the kept-tail boundary, and asserts the MAIN request still satisfies
        // the Anthropic N:N invariant over the WHOLE folded history: every
        // tool_use id has exactly one matching tool_result id (no orphans). A
        // regression that cut mid-pair, or dropped/merged a tail message, would
        // orphan one side and trip the sorted-id equality below.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40); // alone over the 30-char threshold
        // Layout (n=7, strict user/assistant alternation from [0]=user):
        //   [0] user(long)                                  ← head (summary folded in)
        //   [1] asst a1  [2] user u1  [3] asst a2  [4] user u2  ← summarized prefix
        //   [5] asst(tool_use t1, t2)  [6] user(tool_result t1, t2)  ← kept tail
        // keep_recent=2 → find_safe_cut returns cut=5 (messages[5] is the
        // assistant boundary), so the pair stays whole in the tail.
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
            Message::assistant(vec![
                Block::ToolUse {
                    id: "t1".into(),
                    name: "shell".into(),
                    input: json!({"command": "ls"}),
                },
                Block::ToolUse {
                    id: "t2".into(),
                    name: "shell".into(),
                    input: json!({"command": "pwd"}),
                },
            ]),
            Message::user(vec![
                Block::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "ls-out".into(),
                    image: None,
                    is_error: false,
                },
                Block::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "pwd-out".into(),
                    image: None,
                    is_error: false,
                },
            ]),
        ];
        let (prov, state) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", None),      // summarize_prefix call
            mock::resp_text("final answer", None), // main call on folded history
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(state.calls(), 2, "exactly summary call + main call");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Compressed { dropped: 4, kept: 3 })),
            "should drop 4 prefix messages, keep 3 (head + the tail pair)"
        );
        // MAIN (2nd) request: head + asst(2 tool_use) + user(2 tool_result).
        let cap = state.captured();
        let main = &cap[1];
        assert_eq!(main.message_count, 3);
        // N:N bijection over the whole folded history. Anthropic doesn't mandate
        // result order vs. use order, so compare as multisets (sorted).
        let mut uses = main.tool_use_ids.clone();
        let mut results = main.tool_result_ids.clone();
        uses.sort();
        results.sort();
        assert_eq!(
            uses, results,
            "tool_use ↔ tool_result must stay N:N after compression (no orphans)"
        );
        assert_eq!(uses.len(), 2, "both tail tool_uses survived (none dropped)");
    }

    #[test]
    fn compress_summary_request_omits_caching() {
        // The compression summary is a one-off call on a prefix that will never
        // be re-sent as a growing prefix, so it must pass cache:false (no 1.25×
        // write premium). The subsequent main turn is a normal agent turn and
        // passes cache:true. Pinned because the summary builds its own
        // ChatRequest, separate from the turn request — easy to forget.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", None),     // summarize_prefix call
            mock::resp_text("final answer", None), // main call
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        agent.run(&mut |_| {}).unwrap();
        let cap = state.captured();
        assert_eq!(cap.len(), 2);
        assert!(!cap[0].cache, "summary request must be cache:false");
        assert!(cap[1].cache, "main turn request must be cache:true");
    }

    #[test]
    fn compress_summary_usage_is_counted_in_totals() {
        // The summarize_prefix request is a real billed call — its usage must
        // fold into the run totals (and emit a Usage event), not be dropped.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, _) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", mock::mk_usage(200, 30)), // summary call
            mock::resp_text("final", mock::mk_usage(100, 10)),   // main call
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.input_tokens, 300); // 200 (summary) + 100 (main)
        assert_eq!(o.output_tokens, 40); // 30 + 10
        // The summary's usage must emit a Usage event too (not just fold into
        // the totals) — otherwise a future refactor could drop the event emit
        // while the totals still add up and this test would pass. Two calls →
        // two Usage events.
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::Usage { .. }))
                .count(),
            2,
            "summary and main chat each emit a Usage event"
        );
    }

    #[test]
    fn compress_empty_summary_still_counts_its_usage() {
        // The model returned an empty/unusable summary, but the summarize
        // request still billed tokens. Those must be counted (honest cost) even
        // though compression is then disabled and no Compressed event fires.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, _) = MockProvider::new(vec![
            mock::resp_text("   ", mock::mk_usage(200, 30)), // empty summary, but billed
            mock::resp_text("final", mock::mk_usage(100, 10)),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        // Summary usage counted despite the empty/unusable text.
        assert_eq!(o.input_tokens, 300);
        assert_eq!(o.output_tokens, 40);
        // Both calls emitted a Usage event; compression was NOT applied.
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::Usage { .. }))
                .count(),
            2
        );
        assert!(events
            .iter()
            .all(|e| !matches!(e, AgentEvent::Compressed { .. })));
    }

    #[test]
    fn compress_without_provider_usage_emits_no_zero_event() {
        // Compression ran (summary non-empty, history folded, Compressed fires)
        // but the provider reported NO usage for the summary request. The old
        // code synthesized a Usage{0,0} event for it (noise in NDJSON/TUI).
        // G3: no Usage event and nothing added to the totals from the summary —
        // only the main call's usage (which it did report) shows up.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, _) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", None),                  // summary ran, no usage reported
            mock::resp_text("final", mock::mk_usage(100, 10)), // main call reports usage
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        // Totals reflect ONLY the main call — the summary reported no usage, so
        // there is nothing honest to count from it.
        assert_eq!(o.input_tokens, 100);
        assert_eq!(o.output_tokens, 10);
        // Compression did run...
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compressed { dropped: 2, kept: 3 })));
        // ...but emitted exactly ONE Usage event (the main call's), not a
        // spurious second Usage{0,0} for the usage-less summary.
        let usage_events = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Usage { .. }))
            .collect::<Vec<_>>();
        assert_eq!(
            usage_events.len(),
            1,
            "summary with no provider usage must not emit a Usage{{0,0}} event"
        );
        assert!(matches!(
            usage_events[0],
            AgentEvent::Usage { input_tokens: 100, output_tokens: 10, .. }
        ));
    }

    #[test]
    fn compress_failure_degrades_gracefully() {
        // summarize_prefix errors → compress_failed set, NO Compressed event,
        // the run continues uncompressed and still completes.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, _) = MockProvider::new(vec![
            MockTurn::Err("summary blew up".into()), // summarize_prefix fails
            mock::resp_text("answer anyway", None),   // main call proceeds uncompressed
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::Compressed { .. })),
            "no compress event on summary failure"
        );
    }

    #[test]
    fn proactive_compress_on_token_estimate() {
        // max_context_tokens switches preflight to a token estimate (≈4
        // chars/token, reserving this turn's output). A history whose estimate
        // + output exceeds 85% of the window triggers compression BEFORE the
        // main request — same fold / event / short-history guarantees as the
        // char-threshold path, but driven by the model's real window size.
        let mut cfg = cfg_with(5, 0, 0); // char threshold off
        cfg.limits.max_context_tokens = 1000;
        cfg.limits.keep_recent_messages = 2;
        cfg.max_tokens = 100;
        let long = "x".repeat(4000); // est ≈ 1000 tokens >> 85% of 1000 (850)
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", None), // summarize_prefix call
            mock::resp_text("final", None),   // main call on folded history
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(state.calls(), 2);
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compressed { dropped: 2, kept: 3 })));
        let cap = state.captured();
        assert_eq!(cap[1].message_count, 3, "main call saw the folded history");
    }

    #[test]
    fn overflow_error_triggers_force_compress_then_retry() {
        // The core overflow path: the provider rejects the request as too long
        // → the agent force-compresses ONCE and retries the SAME turn on the
        // shorter history, which then succeeds. Pins OpenCode's "compact and
        // retry that step once" recovery — without it, an over-long request
        // fails the whole run instead of self-healing.
        let mut cfg = cfg_with(5, 0, 0); // no preflight; only the overflow path fires
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            MockTurn::Err(
                "OpenAI-compatible API error 400: this model's maximum context length is 8192 tokens"
                    .into(),
            ),
            mock::resp_text("SUMMARY", None),       // force-compress summary call
            mock::resp_text("recovered answer", None), // retried main call
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(o.final_text, "recovered answer");
        // overflow Err → forced summary → retried main. Exactly 3 calls.
        assert_eq!(state.calls(), 3);
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compressed { dropped: 2, kept: 3 })));
        let cap = state.captured();
        assert_eq!(cap[0].message_count, 5, "first (overflowing) call saw full history");
        assert_eq!(cap[2].message_count, 3, "retried call saw the folded history");
    }

    #[test]
    fn overflow_second_time_propagates_as_error() {
        // After one forced compression + retry, a SECOND overflow on the same
        // turn is a real error (OpenCode: "a second overflow after recovery is
        // returned as an error"). overflow_recovered caps recovery at one per
        // turn — no infinite retry loop.
        let mut cfg = cfg_with(5, 0, 0);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, _state) = MockProvider::new(vec![
            MockTurn::Err("OpenAI-compatible API error 400: context_length_exceeded".into()),
            mock::resp_text("SUMMARY", None), // forced compress succeeds
            MockTurn::Err("Anthropic API error 400: prompt is too long: 9000 > 8192".into()),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let err = match agent.run(&mut |_| {}) {
            Err(e) => e,
            Ok(_) => panic!("second overflow must propagate as a run error"),
        };
        assert!(
            format!("{err:#}").contains("prompt is too long"),
            "second overflow surfaced, not swallowed: {err:#}"
        );
    }

    #[test]
    fn non_overflow_error_is_not_compressed() {
        // A provider error that isn't a context-overflow (rate limit, auth,
        // generic 400) must propagate IMMEDIATELY — never trigger a
        // compression. Pins that overflow detection is specific, not "any Err".
        let mut cfg = cfg_with(5, 0, 0);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            MockTurn::Err("OpenAI-compatible API error 429: rate limit exceeded".into()),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let err = match agent.run(&mut |e| events.push(e)) {
            Err(e) => e,
            Ok(_) => panic!("non-overflow error must propagate"),
        };
        assert!(format!("{err:#}").contains("rate limit"));
        assert_eq!(state.calls(), 1, "no summary call for a non-overflow error");
        assert!(events
            .iter()
            .all(|e| !matches!(e, AgentEvent::Compressed { .. })));
    }

    #[test]
    fn overflow_with_no_compressible_head_propagates() {
        // If the history is too short to cut (find_safe_cut None) — e.g. the
        // overflow is driven by tool schemas / system prompt, not conversation —
        // there's nothing to compress: propagate the overflow as a real error
        // rather than looping or synthesizing an empty summary.
        let cfg = cfg_with(5, 0, 0); // keep_recent default 8
        let initial = vec![
            user_text("hi"),
            Message::assistant(vec![Block::Text("hello".into())]),
            user_text("more"),
        ]; // 3 messages <= keep_recent(8)+2 → find_safe_cut None
        let (prov, state) = MockProvider::new(vec![
            MockTurn::Err("Anthropic API error 400: prompt is too long".into()),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let err = match agent.run(&mut |_| {}) {
            Err(e) => e,
            Ok(_) => panic!("overflow with no compressible head must propagate"),
        };
        assert!(format!("{err:#}").contains("prompt is too long"));
        assert_eq!(state.calls(), 1, "no summary call when nothing is compressible");
    }

    #[test]
    fn build_window_transcript_strips_marker_returns_old_summary() {
        // The rolling-summary mechanism: when the head already carries a summary
        // block (a later compression), build_window_transcript must (a) return
        // that prior summary as `old_summary` so the prompt asks for an UPDATE,
        // and (b) strip the marker block from the transcript so it isn't re-fed
        // as raw input. The kept tail (>= cut) must not appear in the transcript.
        let mut head = user_text("the task");
        head.content
            .push(Block::Text(format!("{SUMMARY_MARKER}\nold running summary body")));
        let initial = vec![
            head,
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        // cut=3 → summarize head + [a1, u1]; tail [a2, u2] excluded.
        let (transcript, old) = Agent::build_window_transcript(&initial, 3);
        assert_eq!(old.as_deref(), Some("old running summary body"));
        assert!(!transcript.contains(SUMMARY_MARKER), "marker block stripped");
        assert!(transcript.contains("the task"), "original head text kept");
        assert!(transcript.contains("a1") && transcript.contains("u1"));
        assert!(
            !transcript.contains("a2") && !transcript.contains("u2"),
            "kept tail must not appear in the transcript"
        );
    }

    #[test]
    fn compress_large_window_chunks_then_combines() {
        // A prefix larger than SUMMARY_CHUNK_CHARS is summarized per
        // message-aligned chunk, then the chunk summaries are combined into the
        // final brief. This bounds the per-call ratio (~6:1) so detail survives,
        // and keeps each summary call small enough that it can't itself overflow
        // — which the overflow path relies on, since it force-compresses exactly
        // when the context is huge.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let big = "x".repeat(30_000); // one block alone > SUMMARY_CHUNK_CHARS (24_000)
        let initial = vec![
            user_text(&big),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", None), // chunk 1 summary
            mock::resp_text("SUMMARY", None), // chunk 2 summary
            mock::resp_text("SUMMARY", None), // combine summary
            mock::resp_text("final", None),   // main call on folded history
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert!(!o.final_text.is_empty());
        // 2 chunk summaries + 1 combine + 1 main = 4 calls.
        assert_eq!(state.calls(), 4, "chunk path: per-chunk + combine + main");
        let cap = state.captured();
        // The combine call assembled the chunk summaries under "[segment N]".
        let combine = &cap[2];
        assert!(
            combine.user_texts.iter().any(|t| t.contains("segment")),
            "combine call must assemble chunk summaries: {:?}",
            combine.user_texts
        );
        // Every summary call opts out of caching; the main turn uses it.
        assert!(!cap[0].cache && !cap[1].cache && !cap[2].cache, "summaries cache:false");
        assert!(cap[3].cache, "main call uses prompt caching");
        // The main call saw the folded (short) history.
        assert_eq!(cap[3].message_count, 3);
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compressed { dropped: 2, kept: 3 })));
    }

    #[test]
    fn compress_chunk_failure_disables_for_run() {
        // If one of the chunk/combine summary calls fails, compression is
        // abandoned for this run (compress_failed), NO fold happens, and the run
        // continues uncompressed on the original history. Graceful degradation
        // — a transient summary failure must not break the whole run.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let big = "x".repeat(30_000);
        let initial = vec![
            user_text(&big),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", None),               // chunk 1 ok
            MockTurn::Err("chunk2 summary blew up".into()), // chunk 2 fails
            mock::resp_text("answer anyway", None),         // main proceeds uncompressed
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(o.final_text, "answer anyway");
        assert!(events
            .iter()
            .all(|e| !matches!(e, AgentEvent::Compressed { .. })), "no fold on chunk failure");
        let cap = state.captured();
        assert_eq!(cap[2].message_count, 5, "no fold → main sees the full history");
    }

    #[test]
    fn provider_error_propagates_as_run_error() {
        // A provider Err from the main chat surfaces as run()'s Result::Err —
        // the headless harness then writes a result file + exit 1.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) =
            MockProvider::new(vec![MockTurn::Err("503 service unavailable".into())]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("hi")], test_ctx());
        // Match by hand: RunOutcome isn't Debug, so unwrap_err() won't compile.
        let err = match agent.run(&mut |_| {}) {
            Err(e) => e,
            Ok(_) => panic!("expected a provider error to propagate"),
        };
        assert!(format!("{err:#}").contains("503"));
    }

    #[test]
    fn single_run_retries_transient_then_succeeds() {
        // harness::run_with_retries retries on agent.run() Err: two transient
        // 503s then a clean turn → success, exit 0, attempts==3 in result.json.
        // The injected sleeper is a no-op so the test doesn't wait on backoff.
        let cfg = cfg_with(5, 0, 0);
        let (prov, state) = MockProvider::new(vec![
            MockTurn::Err("503 transient".into()),
            MockTurn::Err("503 transient".into()),
            mock::resp_text("recovered", None),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("go")], test_ctx());
        let dir = std::env::temp_dir().join(format!("ta-retry-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("r.json");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), false, 2, |_| {}).unwrap();
        assert_eq!(code, 0, "recovered → exit 0");
        assert_eq!(state.calls(), 3, "two retries → three attempts");
        let s = std::fs::read_to_string(&out).unwrap();
        assert!(s.contains("\"attempts\": 3"), "attempts not recorded: {s}");
        assert!(s.contains("\"success\": true"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_run_retries_exhausted_exit_1() {
        // All attempts fail: exit 1, attempts==retries+1, result.json carries
        // the last error (stop_reason "error"). Deterministic Ok outcomes are
        // never retried — only Err is, which is exactly what the mock feeds.
        let cfg = cfg_with(5, 0, 0);
        let (prov, state) = MockProvider::new(vec![
            MockTurn::Err("503 one".into()),
            MockTurn::Err("503 two".into()),
            MockTurn::Err("503 three".into()),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("go")], test_ctx());
        let dir = std::env::temp_dir().join(format!("ta-retry2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("r.json");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), false, 2, |_| {}).unwrap();
        assert_eq!(code, 1, "exhausted → exit 1");
        assert_eq!(state.calls(), 3, "retries=2 → 3 total attempts, no more");
        let s = std::fs::read_to_string(&out).unwrap();
        assert!(s.contains("\"attempts\": 3"), "{s}");
        assert!(s.contains("\"success\": false"));
        assert!(s.contains("\"stop_reason\": \"error\""));
        assert!(s.contains("503"), "last error surfaced: {s}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_run_no_retry_when_retries_zero() {
        // retries=0: a single Err → exit 1, attempts==1, no retry attempted.
        let cfg = cfg_with(5, 0, 0);
        let (prov, state) = MockProvider::new(vec![MockTurn::Err("503".into())]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("go")], test_ctx());
        let dir = std::env::temp_dir().join(format!("ta-retry0-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("r.json");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
        assert_eq!(code, 1);
        assert_eq!(state.calls(), 1, "retries=0 → exactly one attempt");
        let s = std::fs::read_to_string(&out).unwrap();
        assert!(s.contains("\"attempts\": 1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_run_cancel_during_backoff_exits_130() {
        // A Ctrl-C during the retry backoff must cancel (exit 130) instead of
        // being swallowed: the next agent.run would clear the flag at entry and
        // proceed, making the first Ctrl-C look like a no-op. The sleeper
        // simulates the signal by flipping the shared cancel flag.
        let cfg = cfg_with(5, 0, 0);
        let (prov, state) = MockProvider::new(vec![
            MockTurn::Err("503".into()),
            MockTurn::Err("not reached".into()),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("go")], test_ctx());
        let cancel = Arc::new(AtomicBool::new(false));
        agent.set_cancel(cancel.clone());
        let dir = std::env::temp_dir().join(format!("ta-retry-cancel-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("r.json");
        let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 2, |_| {
            cancel.store(true, Ordering::Relaxed);
        })
        .unwrap();
        assert_eq!(code, 130, "cancel during backoff → exit 130");
        assert_eq!(state.calls(), 1, "only attempt 1 ran; cancel broke before attempt 2");
        let s = std::fs::read_to_string(&out).unwrap();
        assert!(s.contains("cancelled during retry backoff"), "{s}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalize_adjacent_merges_same_role_neighbors() {
        // [user a][user b] → one user with both blocks; alternation preserved.
        let mut msgs = vec![user_text("a"), user_text("b")];
        normalize_adjacent(&mut msgs);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[0].content.len(), 2);

        // A run of three same-role messages folds into one.
        let mut msgs = vec![user_text("a"), user_text("b"), user_text("c")];
        normalize_adjacent(&mut msgs);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content.len(), 3);

        // Already-alternating history is untouched.
        let mut msgs = vec![user_text("a"), Message::assistant(vec![Block::Text("x".into())])];
        normalize_adjacent(&mut msgs);
        assert_eq!(msgs.len(), 2);

        // Mixed: [u][u][a][a][u] → [u][a][u], block order preserved.
        let mut msgs = vec![
            user_text("1"),
            user_text("2"),
            Message::assistant(vec![Block::Text("a1".into())]),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("3"),
        ];
        normalize_adjacent(&mut msgs);
        assert_eq!(msgs.len(), 3);
        assert_eq!(
            msgs.iter().map(|m| m.role).collect::<Vec<_>>(),
            vec![Role::User, Role::Assistant, Role::User]
        );
        assert_eq!(msgs[0].content.len(), 2); // "1"+"2"
        assert_eq!(msgs[1].content.len(), 2); // "a1"+"a2"
        assert_eq!(msgs[2].content.len(), 1); // "3"
    }

    #[test]
    fn normalize_adjacent_preserves_tool_use_tool_result_pairing() {
        // Folding a user message carrying a tool_result into an adjacent user
        // message must keep the tool_result AFTER its assistant tool_use — they
        // live in different roles and never merge across, so N:N pairing and id
        // survival hold. Locks the claim that the safety net can't corrupt a
        // tool history.
        let tu = Block::ToolUse {
            id: "x".into(),
            name: "sh".into(),
            input: json!({}),
        };
        let tr = Block::ToolResult {
            tool_use_id: "x".into(),
            content: "ok".into(),
            image: None,
            is_error: false,
        };
        let mut msgs = vec![
            Message::assistant(vec![tu]),
            Message::user(vec![tr]),
            Message::user(vec![Block::Text("more".into())]),
        ];
        normalize_adjacent(&mut msgs);
        assert_eq!(msgs.len(), 2, "two adjacent user messages fold into one");
        assert_eq!(msgs[0].role, Role::Assistant);
        assert_eq!(msgs[1].role, Role::User);
        // tool_use still leads, in the assistant message; tool_result + text
        // merged into the trailing user message, order preserved.
        assert!(matches!(
            &msgs[0].content[0],
            Block::ToolUse { id, .. } if id == "x"
        ));
        assert_eq!(msgs[1].content.len(), 2);
        assert!(matches!(
            &msgs[1].content[0],
            Block::ToolResult { tool_use_id, .. } if tool_use_id == "x"
        ));
        assert!(matches!(&msgs[1].content[1], Block::Text(t) if t == "more"));
    }

    #[test]
    fn run_normalizes_adjacent_user_messages_before_request() {
        // A resumed/hand-edited history with two trailing user messages would
        // make Anthropic 400. The loop must fold them into one before the
        // first chat — assert the provider sees strictly alternating roles.
        let cfg = cfg_with(5, 0, 0);
        let (prov, state) = MockProvider::new(vec![mock::resp_text("done", None)]);
        // Two adjacent user messages at the head.
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![],
            vec![user_text("first"), user_text("second")],
            test_ctx(),
        );
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "end_turn");

        let cap = state.captured();
        assert_eq!(cap.len(), 1, "exactly one chat call");
        let roles = &cap[0].roles;
        assert_eq!(
            roles,
            &vec![Role::User],
            "two adjacent user messages were folded into one before the request"
        );
        assert_eq!(cap[0].message_count, 1);
        // No two adjacent roles equal (trivially true here, but pins the rule).
        for w in roles.windows(2) {
            assert_ne!(w[0], w[1], "adjacent same-role leaked: {roles:?}");
        }
    }
}
