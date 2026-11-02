//! Optional third-party interception layer (P1).
//!
//! Lets library users observe and gate what an agent does without touching the
//! core: approve / deny / rewrite each tool call before it runs, audit results
//! after, and watch every outgoing chat request. Install on a built agent with
//! [`crate::Agent::set_hooks`]; `task` sub-agents inherit the layer
//! automatically (via [`ToolCtx::hooks`]).
//!
//! ```no_run
//! # use tiny_agent::{build_agent, Agent, Config, Hooks, ToolDecision};
//! # use std::sync::Arc;
//! struct Policy;
//! impl Hooks for Policy {
//!     fn before_tool(&self, tool: &str, _input: &serde_json::Value, _ctx: &tiny_agent::ToolCtx) -> ToolDecision {
//!         if tool == "shell" { ToolDecision::Deny("no shell in this deployment".into()) }
//!         else { ToolDecision::Allow }
//!     }
//! }
//! # fn main() -> anyhow::Result<()> {
//! # let cfg = Config::load(std::path::Path::new("chat.json"))?;
//! # let client = tiny_agent::provider::new_client(cfg.limits.timeout_secs);
//! let mut agent = build_agent(&cfg, Vec::new(), &client)?;
//! agent.set_hooks(Arc::new(Policy));
//! # Ok(())
//! # }
//! ```

use crate::provider::ChatRequest;
use crate::tools::{ToolCtx, ToolOutput};
use serde_json::Value;

/// What a [`Hooks::before_tool`] gate decides for one pending tool call.
///
/// Producer-side enum (third parties CONSTRUCT it, the core consumes) —
/// deliberately **not** `#[non_exhaustive]` (see `AgentEvent`'s doc for the
/// hygiene rule): constructors must not be forced into wildcard arms.
#[derive(Debug, Clone)]
pub enum ToolDecision {
    /// Run the tool with the model's original input (the default).
    Allow,
    /// Refuse to run. The string becomes the tool_result text, marked
    /// `is_error` — write it for the model, it is what the model sees. The
    /// call still pairs N:N with its tool_use and lands in the tool-call log
    /// (`is_error: true`), so the run's bill and audit stay complete.
    Deny(String),
    /// Run the tool with REPLACED input. The audit trail (tool-call log,
    /// `ToolCall` events) keeps the model's ORIGINAL input; only the executed
    /// call sees the replacement. That split is deliberate: the log answers
    /// "what did the model ask for", the output answers "what actually
    /// happened".
    Modify(Value),
}

/// What a [`Hooks::on_turn_stopping`] arbitration decides for a run the
/// model is about to end with its own `end_turn`.
///
/// Producer-side enum (third parties CONSTRUCT it, the core consumes) —
/// deliberately **not** `#[non_exhaustive]` (same hygiene rule as
/// [`ToolDecision`]): constructors must not be forced into wildcard arms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnStopDecision {
    /// Let the run end exactly as it would have without the hook (the
    /// default — an implementation that never overrides
    /// `on_turn_stopping` leaves every run byte-identical).
    Stop,
    /// Reject this completion and force one more turn. `note` is queued
    /// through the supervisor-note delivery pipeline (the same marked block,
    /// FIFO, at-most-once, panic-safe machinery as
    /// [`crate::Agent::supervisor_handle`]) and reaches the model before the
    /// next request. Continuations are capped per run — see
    /// [`TurnStopInfo::continuations_cap`]; past the cap the decision
    /// degrades to `Stop` with `stop_reason = "arbitration_capped"`.
    Continue {
        /// Text merged into the tail user turn as a marked block for the
        /// model (why the completion was rejected / what to do next).
        note: String,
    },
}

/// The context one turn-stopping arbitration sees. Core-constructed,
/// hook-read — an implementation never constructs it.
pub struct TurnStopInfo<'a> {
    /// The text blocks of the turn that is ending the run, joined with
    /// newlines — the completion claim being judged. Empty when the final
    /// turn carried no visible text (e.g. a thinking-only reply).
    pub reply_text: &'a str,
    /// 1-based number of the turn about to end the run.
    pub turn: u32,
    /// How many `Continue` decisions this run has already honored (the
    /// decision currently being made is number `continuations_used + 1`).
    pub continuations_used: u32,
    /// The per-run cap on `Continue` decisions — a `Continue` past it is
    /// downgraded to `Stop` and the run ends with
    /// `stop_reason = "arbitration_capped"`. Aligned with the stuck-loop
    /// counter discipline (same value as the private `STUCK_STOP_AT`, 6).
    pub continuations_cap: u32,
}

/// Third-party interception points into the agent loop. All methods default
/// to no-ops, so an implementation overrides only what it needs. The layer is
/// installed per agent via [`crate::Agent::set_hooks`]; `task` sub-agents
/// inherit it through [`ToolCtx::hooks`].
///
/// # Call order & concurrency — read before implementing
///
/// **Order within one turn.** `before_tool` runs for every Sequential
/// (inline) call first, in block order, then for every Parallel-batch call,
/// in block order — with a mixed turn that is NOT the same as global block
/// order. `after_tool` mirrors it: Sequential calls fire inline right after
/// each execution; Parallel calls fire after the whole batch joins (block
/// order); a denied call fires at decision time.
///
/// **Concurrency.** The TOP-LEVEL gates are strictly sequential — they run on
/// the parent thread before any worker spawns, so approval decisions never
/// race each other. BUT a `task` sub-agent inherits this layer, and a
/// sub-agent's OWN gates (its leaf tools' `before_tool`/`after_tool` and its
/// `on_chat_request`) run on that sub-agent's worker thread. Sibling
/// sub-agents are genuinely concurrent, so all three methods may be called
/// from several threads at once. `Send + Sync` only makes handing the trait
/// object across threads memory-safe — the implementation must be thread-safe
/// by itself (interior mutability + synchronization, e.g. `Mutex`/atomics).
///
/// **Scope.** `on_chat_request` observes MAIN-turn requests only; the
/// compression summary request is deliberately not surfaced (it's an internal
/// housekeeping call, and rewriting it would need summary-consistency
/// semantics this layer doesn't define). Likewise, `run_code`'s
/// `tools.list()` self-introspection (code feature) bypasses the gate —
/// listing which tools exist is not a side-effecting policy surface, while
/// every `tools.call(...)` from the sandbox DOES gate, with
/// `ctx.subagent == Some("__code__")` so a policy can treat code-originated
/// calls apart from top-level and `task` ones.
///
/// **Threat model.** This layer is a compile-time extension point for the
/// embedder's own approval / policy system — NOT a built-in security
/// boundary. It gates the calls the MODEL makes; it cannot protect against
/// a malicious tool implementation (tools and hooks are compile-time trusted
/// code, running in-process with the same privileges). For actual isolation,
/// run the agent in a container or under a restricted user.
///
/// **Edges.**
/// - Unknown tool names still pass the gate; `after_tool` then reports
///   `executed = true` for the attempted run whose result is the "unknown
///   tool" error (executed = "reached the run attempt", not "tool exists").
///   Same for a Parallel worker that PANICKED: the loop synthesizes an error
///   output for it and reports `executed = true` — a third flavour of
///   "attempted but failed" alongside tool Err results (`executed = false`
///   is reported only for a `before_tool` Deny and for a call skipped
///   because the run's cancel flag landed while the gate was deliberating
///   — the gate→execution checkpoint re-checks cancel so an approval wait
///   cannot still execute the tool it approved — or, in the parallel batch,
///   for an approved call that never started its tool body: the worker's
///   pre-body re-check, the pre-spawn checkpoint, and the wave-start
///   checkpoint all convert it to the same recorded
///   `after_tool(executed = false)` placeholder, so a passed gate never
///   leaves an unmatched `before_tool`. Calls the gate loop never reached
///   (cancel mid-gating) have no `before_tool` and stay unrecorded.)
/// - One more pairing gap, from `run_code`'s own interception layer (code
///   feature): a call the sandbox's deny_paths / fs_write_dirs rules reject
///   passes `before_tool` (Allow) but never reaches `after_tool` — the
///   rejection is a sandbox policy decision, not a hooks one, so
///   `executed = false` stays reserved for `before_tool` Deny (and gate-time
///   cancel, including the parallel worker's pre-body re-check). Audit-style
///   hooks that pair before/after counts will see these as unmatched
///   `before_tool`s; the rejection itself lands in the sandbox's `meta`
///   audit trace.
/// - A panicking HOOKS method is not caught anywhere: on the parent thread
///   (top-level gate / after-notify / chat observation) it unwinds through
///   `Agent::run` and may leave the already-saved session with an unpaired
///   tool_use — the same consequence as a Sequential tool's `run`
///   panicking (hooks are a new panic SOURCE at existing call sites, not a
///   new panic CLASS). Inside a sub-agent it stays on that worker thread and
///   the parent batch's join absorbs it as the "parallel tool worker
///   panicked" error output. Return `Deny` on bad input rather than
///   panicking.
/// - `Modify` on a `todo` call changes only what EXECUTES; the `plan` field
///   in `result.json` is derived from the recorded (original) input — the
///   plan reflects what the model wanted to send. A denied `todo` is recorded
///   `is_error` and so never enters the plan.
/// - An agent rebuilt from `--resume` does NOT carry hooks over (the layer
///   lives on the agent instance, not in the session file) — callers must
///   `set_hooks` again after rebuilding.
pub trait Hooks: Send + Sync {
    /// Gate a tool call before execution. `tool` is the tool's name, `input`
    /// the model's original arguments, `ctx` the execution context (inspect
    /// [`ToolCtx::subagent`] to tell top-level calls from sub-agent calls).
    /// Returns a [`ToolDecision`]; the default allows everything.
    fn before_tool(&self, tool: &str, input: &Value, ctx: &ToolCtx) -> ToolDecision {
        let _ = (tool, input, ctx);
        ToolDecision::Allow
    }

    /// Observe a finished tool call: the ORIGINAL input the model sent (never
    /// a `Modify` replacement), the produced output, whether it executed
    /// (`false` for a [`Hooks::before_tool`] Deny, for a call skipped at the
    /// gate→execution cancel checkpoint, and for an
    /// approved-but-never-started parallel placeholder — see *Edges* below),
    /// and the execution context — mirror of [`Hooks::before_tool`]'s `ctx`, so an
    /// audit log can attribute the call to its source
    /// ([`ToolCtx::subagent`]: top-level, a `task` child, or the `run_code`
    /// sandbox). Attribution CANNOT be recovered from call order: parallel
    /// siblings genuinely interleave, so the per-call ctx is the only
    /// reliable source tag. Default no-op.
    ///
    /// BREAKING vs 0.3.x: the `ctx` parameter is new. An implementation
    /// still using the old 4-parameter signature compiles (it degenerates
    /// into an inherent method) but SILENTLY stops overriding this trait
    /// method — the default no-op takes over and the hook fires no more.
    /// Migration: append `ctx: &ToolCtx` to the parameter list.
    fn after_tool(
        &self,
        tool: &str,
        input: &Value,
        output: &ToolOutput,
        executed: bool,
        ctx: &ToolCtx,
    ) {
        let _ = (tool, input, output, executed, ctx);
    }

    /// Observe each outgoing main-turn chat request (read-only). Default
    /// no-op. NB: a cancel that lands DURING the callback does not abort
    /// this request — the observation point has no cancel re-check; the
    /// flag is honored at the next turn boundary (deliberate: the request
    /// is already committed by the time the observer returns).
    fn on_chat_request(&self, req: &ChatRequest) {
        let _ = req;
    }

    /// Turn-end arbitration (the host RSI contract): called each time the
    /// TOP-LEVEL run is about to end because the model's last turn finished
    /// with its own `end_turn` — the one stop a host may veto in the
    /// moment, turning "the model claims done" into "one more turn with
    /// this note". Every other exit is a forced termination and never
    /// consults this hook: `max_tokens` (incl. the salvage path),
    /// `stuck_loop`, cancel, `budget_exceeded`, `max_turns_reached`,
    /// timeouts, and every `Err` return.
    ///
    /// Semantics, read before implementing:
    /// - **Default `Stop` = zero drift.** A run whose hook returns `Stop`
    ///   (or never overrides this method) is byte-identical to a run
    ///   without the hook — same history, same events, same stop_reason.
    ///   The [`crate::AgentEvent::TurnArbitration`] audit event is emitted
    ///   only for non-default decisions (a `Continue` honored, or one
    ///   downgraded at the cap), so pipelines that don't opt in see nothing.
    /// - **`Continue` reuses the supervisor-note pipeline** (R1): the note
    ///   is queued and delivered as a marked block into the tail user turn
    ///   right before the NEXT request — the model sees it there; a
    ///   `NoteInjected` event with `deferred == false` follows at that
    ///   delivery point. If the run dies before the next request (e.g. a
    ///   cancel landing in the window), the queued note is flushed at run
    ///   exit as `deferred == true` — late, never lost (the R1 contract).
    /// - **Cap.** At most [`TurnStopInfo::continuations_cap`] `Continue`
    ///   decisions per run; past it the decision is downgraded to `Stop`,
    ///   `stop_reason` becomes `"arbitration_capped"` (a core-level label),
    ///   and the note survives only in the `TurnArbitration` event — the
    ///   run ends cleanly, history untouched by the discarded note.
    /// - **Scope.** Top-level runs only: a `task` sub-agent inherits this
    ///   layer for gating/observation, but its run never arbitrates — a
    ///   child's completion is the `task` call's concern, and the
    ///   serial/no-delegation turn-stopping shape is a property of the
    ///   observed run, not of every nested one.
    /// - **Cancel still wins.** A cancel flagged during (or after) a
    ///   `Continue` lands at the next loop-top check — the continuation
    ///   never sends a request past it, `stop_reason = "cancelled"`.
    /// - **Ordering vs compression.** Arbitration happens after the final
    ///   turn's messages are persisted (the turn's message face is
    ///   settled); the injected note is delivered at the next turn top,
    ///   before that turn's preflight compression — so the note's bytes
    ///   are counted by the token precheck like any tail content.
    /// - **Errors fail safe.** On any internal error return `Stop`, not a
    ///   panic: a panicking hook is not caught anywhere (same discipline as
    ///   the other hooks — see *Edges*) and would unwind `Agent::run`
    ///   instead of ending the run cleanly.
    /// - An agent rebuilt from `--resume` does NOT carry hooks over (see
    ///   *Edges*), so a resumed run arbitrates only after the host re-runs
    ///   `set_hooks`.
    fn on_turn_stopping(&self, info: &TurnStopInfo) -> TurnStopDecision {
        let _ = info;
        TurnStopDecision::Stop
    }
}
