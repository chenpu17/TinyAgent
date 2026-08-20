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
/// semantics this layer doesn't define).
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
///   "attempted but failed" alongside tool Err results (only a `before_tool`
///   Deny reports `executed = false`).
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
    /// a `Modify` replacement), the produced output, and whether it executed
    /// (`false` only when [`Hooks::before_tool`] denied it). Default no-op.
    fn after_tool(&self, tool: &str, input: &Value, output: &ToolOutput, executed: bool) {
        let _ = (tool, input, output, executed);
    }

    /// Observe each outgoing main-turn chat request (read-only). Default
    /// no-op.
    fn on_chat_request(&self, req: &ChatRequest) {
        let _ = req;
    }
}
