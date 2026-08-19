use crate::config::{Config, Limits};
use crate::compress;
use crate::provider::{Block, ChatEvent, ChatRequest, Message, Provider, Role, ToolDef, Usage};
use crate::tools::{Tool, ToolConcurrency, ToolCtx, ToolOutput};
use anyhow::Result;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Per-request timeout used as a leak backstop when the run has no deadline
/// (`limits.timeout_secs == 0`). The `sse-reader` worker thread is blocked in
/// a reqwest `read()` it can't observe the cancel flag during, so without ANY
/// per-request timeout a half-open connection leaves it lingering forever
/// after the caller has already given up (idle/cancel) — the worker thread and
/// its TCP connection leak until process exit. reqwest's blocking client
/// exposes no idle-read timeout, so a total per-request cap is the only
/// available guarantee the reader thread exits.
///
/// Generous on purpose: a single chat response taking longer than this is
/// effectively a stuck connection, not a legit generation. It caps ONE
/// request, never the RUN (a run is many requests) — so "no timeout" still
/// means an unbounded run, just not an unbounded single request. A run with a
/// real budget (`timeout_secs > 0`) ignores this and derives per-request from
/// the remaining budget instead.
const STREAM_BACKSTOP: Duration = Duration::from_secs(10 * 60);

/// What triggered a history compression: the per-turn preflight estimate
/// crossing the configured threshold, or overflow recovery after the
/// provider rejected a request as too long for the context window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum CompressTrigger {
    Preflight,
    Overflow,
}

/// Events emitted during a run; frontends (headless printer / TUI) consume them.
///
/// `Serialize` is derived so the Python / TypeScript bindings can hand each event
/// to host code as a native dict / object (via `pythonize` / napi serde interop)
/// without a separate DTO. The recursive `SubAgent` variant serializes fine.
///
/// `#[non_exhaustive]`: variants will keep being added as the agent grows;
/// external `match`es must keep a wildcard arm (add one `_ => {}` when
/// upgrading). Inside this crate the attribute is inert — exhaustive matches
/// stay enforced, and a new variant still pings every in-crate consumer at
/// compile time. Hygiene note: only CONSUMER-side enums (third parties match
/// on them) get this attribute; producer-side enums (third parties construct,
/// the core consumes) deliberately do not.
#[derive(Debug, serde::Serialize)]
#[non_exhaustive]
pub enum AgentEvent {
    TurnStart(u32),
    TextDelta(String),
    /// Reasoning/thinking increment from a reasoning model — serializes as
    /// `{"ThinkingDelta": "..."}` so a downstream UI (TinyWorker) renders a
    /// collapsible "thinking" block. Not part of the reply text nor history.
    ThinkingDelta(String),
    ToolCall { name: String, input: Value },
    ToolResult { name: String, is_error: bool, content: String },
    /// The agent updated its self-tracked task plan via the `todo` tool (full
    /// replacement each call). Emitted by the `todo` tool itself, so `--json`
    /// pipelines can render live progress; serializes as `{"TodoUpdate":
    /// {"todos":[...]}}`. The final plan is also derived into `result.json`'s
    /// `plan` field at run end.
    TodoUpdate { todos: Vec<TodoItem> },
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolCallRecord {
    pub name: String,
    pub input: Value,
    /// The tool's result text (what read_file returned, shell stdout, …) — kept
    /// so the structured result file is self-contained for verification, not
    /// just the model's summary of it.
    pub output: String,
    pub is_error: bool,
    /// Structured audit metadata the tool attached (not shown to the model).
    /// Only the `task` sub-agent sets this today (a compact child-run trace).
    /// Serialized as `tool_calls[].meta` in `result.json` (key omitted when
    /// None, so leaf tools stay byte-identical to before).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// One row of the agent's self-tracked task plan, written via the `todo` tool.
/// The `todo` tool uses full-replacement semantics (each call sends the entire
/// plan), so the authoritative final plan is simply the input of the last
/// non-error `todo` call in the run's `tool_calls` log — see `extract_plan`.
/// That log is independent of conversation compression (compression rewrites
/// `messages`, never the `records` accumulator), so the derived plan survives
/// even when the `todo` tool_results in history are folded into the summary.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct TodoItem {
    pub subject: String,
    /// `"pending"` | `"in_progress"` | `"completed"` — stored as a string (not
    /// an enum) so the field round-trips to JSON as-is and stays tolerant of
    /// whatever the model sent (validation lives in the tool, not here).
    pub status: String,
}

/// Derive the run's final task plan from its tool-call log: the input of the
/// **last** `todo` call that did not error. Returns `None` when the model never
/// used `todo` (or only ever errored), in which case `result.json` omits the
/// `plan` field entirely. A free function rather than an `Agent` field because
/// the `records` log already carries everything and already flows untouched
/// through retry / failure / batch — no state machine, no compression hazard.
pub fn extract_plan(records: &[ToolCallRecord]) -> Option<Vec<TodoItem>> {
    let last = records
        .iter()
        .rev()
        .find(|r| r.name == "todo" && !r.is_error)?;
    parse_todos(&last.input)
}

/// Parse a `todo` tool input (`{"todos": [{subject, status}, …]}`) into typed
/// items. Defensive: any malformation (missing array, bad object shape, wrong
/// status value) yields `None` so a garbled plan never poisons `result.json`.
/// The `todo` tool validates the same way before running, so a record that
/// reaches here as non-error is already well-formed — this is the belt to that
/// suspenders (e.g. a hand-constructed record in tests).
fn parse_todos(input: &Value) -> Option<Vec<TodoItem>> {
    let arr = input.get("todos")?.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let subject = v.get("subject")?.as_str()?.trim();
        if subject.is_empty() {
            return None;
        }
        let status = v.get("status")?.as_str()?;
        if !matches!(status, "pending" | "in_progress" | "completed") {
            return None;
        }
        out.push(TodoItem {
            subject: subject.to_string(),
            status: status.to_string(),
        });
    }
    Some(out)
}

#[derive(serde::Serialize)]
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

/// A run that failed after burning tokens / running tools. Carries the partial
/// cost so a retry loop can bill the failed attempt honestly: its tokens were
/// spent (the provider streamed them before erroring) and its tool calls
/// already had their side effects (files written, shells run — they're in the
/// history and on disk). Charging only the final successful attempt in
/// `result.json` understates the true cost, which is why `run_with_retries`
/// and batch `run_one` fold this back into the outcome they report.
///
/// `Display`/`Debug` delegate to `source`, so existing `{e}` / `{e:#}`
/// formatting at call sites is unchanged.
pub struct RunError {
    pub source: anyhow::Error,
    /// Tokens burned by chat requests in this run before it errored.
    pub usage: Usage,
    /// Tool calls executed in this run before it errored.
    pub tool_calls: Vec<ToolCallRecord>,
    /// Turns completed before the error — surfaced in result.json on failure so
    /// the schema matches the success path (a CI consumer reads `.turns` /
    /// `.elapsed_secs` regardless of `.success`).
    pub turns: u32,
    /// Compute time elapsed before the error.
    pub elapsed: Duration,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.source, f)
    }
}
impl std::fmt::Debug for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.source, f)
    }
}
impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Serialize the structured cost fields (usage / tool_calls / turns / elapsed)
/// so the Python / TypeScript bindings can hand a failed run's honest cost back
/// to the host — previously the bindings only took `Display` (the source
/// message), dropping the tokens/tools/turns that CLAUDE.md's "重试诚实成本"
/// design gathered them for. `source` (anyhow::Error) serializes as its Display
/// string since anyhow has no Serialize impl; Display/Debug above are unchanged
/// so core call-site `{e}` formatting is byte-identical.
impl serde::Serialize for RunError {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = ser.serialize_struct("RunError", 5)?;
        s.serialize_field("error", &self.source.to_string())?;
        s.serialize_field("usage", &self.usage)?;
        s.serialize_field("tool_calls", &self.tool_calls)?;
        s.serialize_field("turns", &self.turns)?;
        s.serialize_field("elapsed", &self.elapsed)?;
        s.end()
    }
}

/// Accumulated partial run cost (tokens + tool calls) from failed attempts,
/// folded into a successful outcome so `result.json` bills every attempt
/// honestly (P2-3). Shared between single-run `run_with_retries` and batch
/// `run_one`, which both accumulate each failed attempt's `RunError` cost and
/// fold it into the final outcome they report.
#[derive(Default, Clone)]
pub struct RunCost {
    pub usage: Usage,
    pub tool_calls: Vec<ToolCallRecord>,
    /// Turns accumulated across failed attempts (folded into the outcome so
    /// `result.json`'s `turns`/`elapsed_secs` stay consistent on success).
    pub turns: u32,
    pub elapsed: Duration,
}

impl RunCost {
    /// Add a failed attempt's partial cost: the tokens it burned and the tools
    /// it ran (whose side effects already landed in the history / on disk).
    /// `&RunError` (not by value) so the caller can still format the error.
    pub(crate) fn add_failed(&mut self, re: &RunError) {
        self.usage.input_tokens += re.usage.input_tokens;
        self.usage.output_tokens += re.usage.output_tokens;
        self.usage.cache_creation_input_tokens += re.usage.cache_creation_input_tokens;
        self.usage.cache_read_input_tokens += re.usage.cache_read_input_tokens;
        self.tool_calls.extend(re.tool_calls.iter().cloned());
        self.turns += re.turns;
        self.elapsed += re.elapsed;
    }

    /// Fold this accumulated cost into a successful outcome so its reported
    /// totals cover EVERY attempt. Tool calls from earlier (failed) attempts
    /// come first — they happened first chronologically.
    pub(crate) fn fold_into(&mut self, o: &mut RunOutcome) {
        o.input_tokens += self.usage.input_tokens;
        o.output_tokens += self.usage.output_tokens;
        o.cache_creation_input_tokens += self.usage.cache_creation_input_tokens;
        o.cache_read_input_tokens += self.usage.cache_read_input_tokens;
        // Earlier attempts first, then this attempt's tool calls.
        self.tool_calls.append(&mut o.tool_calls);
        o.tool_calls = std::mem::take(&mut self.tool_calls);
        o.turns += self.turns;
        o.elapsed += self.elapsed;
    }
}

pub struct Agent {
    provider: Box<dyn Provider>,
    /// The protocol family declared by the config ("anthropic" / "openai") —
    /// NOT derived from the boxed `provider` above. When a caller injects a
    /// custom provider (bindings' provider= callback) this still records what
    /// protocol the CONFIG declared, which is what result.json attributes the
    /// bill to. Kept as a plain string (via `ProviderKind::as_str`) so it
    /// serializes/reports without carrying the enum around.
    provider_label: String,
    tools: Vec<Box<dyn Tool>>,
    tool_defs: Vec<ToolDef>,
    messages: Vec<Message>,
    system: Option<String>,
    model: String,
    max_tokens: u32,
    /// Reasoning effort forwarded to the provider verbatim (None = default).
    reasoning_effort: Option<String>,
    limits: Limits,
    ctx: ToolCtx,
    session_path: Option<std::path::PathBuf>,
    /// Set by the frontend (e.g. TUI Esc) to stop at the next turn boundary.
    /// This is the RECOVERABLE per-turn interrupt: the next turn reuses the
    /// same agent, tools, and (notably) the same MCP server child. So it must
    /// NEVER tear down an irreversible resource.
    cancel: Option<Arc<AtomicBool>>,
    /// Separate teardown signal, armed on tools via `set_shutdown`. The frontend
    /// sets this ONLY on session end (TUI quit), never on a per-turn cancel.
    /// Tools that own un-interruptible blocking resources (MCP stdio servers)
    /// arm a watcher on it to kill their child — unblocking a stuck call so the
    /// agent can drop. Split from `cancel` so a recoverable turn-interrupt does
    /// not irreversibly kill the MCP server (no restart) the audit found.
    shutdown: Option<Arc<AtomicBool>>,
    /// One compression failure disables further attempts for this run,
    /// so a persistent error doesn't retry (and warn) every single turn.
    compress_failed: bool,
    /// Running token totals for the current run (reset at run_impl entry).
    /// Kept on self — not a local — so `run_error` can snapshot the partial
    /// cost into the returned `RunError` when a run fails mid-way, letting a
    /// retry loop bill the failed attempt's burned tokens (honest cost).
    usage: Usage,
    /// Pre-serialized size of the tool definitions (name + description +
    /// schema JSON). tool_defs never changes after construction, so
    /// estimate_tokens would otherwise re-serialize every schema on EVERY
    /// turn's preflight check.
    tool_schema_chars: usize,
}

/// Build the system prompt with a leading `# Environment` block (OS / Shell /
/// working directory). `os` and `shell` are parameters — not hardcoded — so
/// the cross-platform shape is unit-testable without actually running on each
/// target. When `base` is `None` (or empty) the env block stands alone.
pub(crate) fn system_with_env(
    base: Option<&str>,
    os: &str,
    shell: &str,
    workdir: &str,
) -> Option<String> {
    let env = format!(
        "# Environment\n- OS: {os}\n- Shell: {shell}\n- Working directory: {workdir}"
    );
    Some(match base {
        Some(b) if !b.is_empty() => format!("{env}\n\n{b}"),
        _ => env,
    })
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
        // Prepend runtime environment facts (OS / Shell / cwd) to the system
        // prompt so the model emits platform-correct commands (e.g. it learns
        // it's under `cmd /C`, not bash, and stops writing `$((...))` that
        // silently no-ops and exits 0). Done here — the single construction
        // chokepoint — so BOTH the main agent and every `task` sub-agent (which
        // overwrites system_prompt then calls Agent::new) get it. `os`/`shell`
        // are args to the pure helper so the cross-platform shape is testable
        // off-platform; read `ctx.workdir` before it is moved below.
        let system = if cfg.inject_environment {
            system_with_env(
                cfg.system_prompt.as_deref(),
                std::env::consts::OS,
                if cfg!(unix) { "sh -c" } else { "cmd /C" },
                &ctx.workdir.display().to_string(),
            )
        } else {
            cfg.system_prompt.clone()
        };
        Self {
            provider,
            provider_label: cfg.provider.as_str().to_string(),
            tools,
            tool_defs,
            messages: initial,
            system,
            model: cfg.model.clone(),
            max_tokens: cfg.max_tokens,
            reasoning_effort: cfg.reasoning_effort.clone(),
            limits: cfg.limits.clone(),
            ctx,
            session_path: None,
            cancel: None,
            shutdown: None,
            compress_failed: false,
            usage: Usage::default(),
            tool_schema_chars,
        }
    }

    // ── Read-only accessors (state for library users / bindings) ─────────────

    /// Conversation history accumulated so far (user / assistant / tool_result
    /// blocks). Multi-turn library callers read this to persist or inspect state
    /// between [`Agent::run`] calls.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Running token totals for the current/latest run. Reset at the start of
    /// each [`Agent::run`]; after a run returns it holds that run's summed usage.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// The model name this agent sends with each request (from the config).
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The protocol family declared by the config ("anthropic" / "openai") —
    /// what result.json attributes the bill to. With a custom injected
    /// provider (bindings' provider= callback) this is still the config's
    /// declared kind; see the field doc for why.
    pub fn provider(&self) -> &str {
        &self.provider_label
    }

    /// The limits (max_turns / timeout / token budget / …) in effect.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Attach the RECOVERABLE per-turn cancellation flag; checked at every
    /// turn boundary and shared with tools (via ToolCtx) so a running shell can
    /// be interrupted too, not only the streaming read.
    ///
    /// Deliberately does NOT arm teardown on tools. The per-turn cancel is
    /// recoverable — the next turn reuses the same agent and the same MCP server
    /// child — so killing the server here would be an irreversible defect.
    /// Teardown (for tools that need it, e.g. MCP stdio servers) is armed on the
    /// separate `set_shutdown` signal, which the frontend fires ONLY on session
    /// end. (P1 audit fix: previously this propagated `cancel` to the MCP
    /// reaper, so a TUI Esc killed the server the next turn couldn't reuse.)
    pub fn set_cancel(&mut self, flag: Arc<AtomicBool>) {
        self.ctx.cancel = Some(flag.clone());
        self.cancel = Some(flag);
    }

    /// Attach the session-end TEARDOWN flag and arm shutdown-aware teardown on
    /// tools that own un-interruptible blocking resources (MCP stdio servers):
    /// on shutdown they kill their child so a stuck call unblocks and the agent
    /// can drop — otherwise the child is orphaned when the frontend stops
    /// waiting (TUI quit). This is distinct from the per-turn `cancel`: shutdown
    /// is irreversible, so the frontend must fire it ONLY on session end.
    pub fn set_shutdown(&mut self, flag: Arc<AtomicBool>) {
        for t in &mut self.tools {
            t.set_shutdown(&flag);
        }
        self.shutdown = Some(flag);
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

    /// Wrap a mid-run error with the partial cost burned so far (this run's
    /// accumulated `usage` + the `records` of tools already executed), so a
    /// retry loop can bill the failed attempt honestly. Called at every
    /// error-return point in `run_impl`.
    fn run_error(
        &self,
        source: anyhow::Error,
        records: &[ToolCallRecord],
        turns: u32,
        elapsed: Duration,
    ) -> RunError {
        RunError {
            source,
            usage: self.usage,
            tool_calls: records.to_vec(),
            turns,
            elapsed,
        }
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
    pub fn run(&mut self, events: &mut dyn FnMut(AgentEvent)) -> Result<RunOutcome, RunError> {
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
    ) -> Result<RunOutcome, RunError> {
        self.run_impl(events, /* reset_cancel */ false)
    }

    fn run_impl(
        &mut self,
        events: &mut dyn FnMut(AgentEvent),
        reset_cancel: bool,
    ) -> Result<RunOutcome, RunError> {
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
            // Token budget guard: refuse to start another turn once the
            // accumulated spend (every turn + sub-agent + compression call)
            // has crossed the cap. Turn-boundary semantics, matching
            // timeout_secs / max_turns — the turn that blew past the cap
            // already finished (its tools ran, its assistant message +
            // tool_results are in history); this only prevents a fresh turn
            // from starting, so the transcript and N:N tool pairing stay
            // intact. 0 = no cap.
            if self.limits.max_total_tokens > 0
                && self.usage.total() >= self.limits.max_total_tokens
            {
                stop_reason = "budget_exceeded".into();
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
                        per_request_timeout: Some(
                            deadline
                                .map(|d| d.saturating_duration_since(Instant::now()))
                                .unwrap_or(STREAM_BACKSTOP),
                        ),
                        // Shared cancel flag: the streaming reader checks it
                        // between chunks, so Esc/Ctrl-C can interrupt a stuck
                        // request mid-read (not only at the next turn boundary).
                        cancel: self.cancel_flag(),
                        // The main turn benefits from prompt caching (re-reads
                        // the stable prefix across turns); one-off requests opt
                        // out.
                        cache: true,
                        reasoning_effort: self.reasoning_effort.as_deref(),
                    };
                    self.provider.chat(&req, &mut |e| match e {
                        ChatEvent::TextDelta(t) => events(AgentEvent::TextDelta(t)),
                        ChatEvent::ThinkingDelta(t) => events(AgentEvent::ThinkingDelta(t)),
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
                        return Err(self.run_error(e, &records, turns, start.elapsed()));
                    }
                    Err(e) => return Err(self.run_error(e, &records, turns, start.elapsed())),
                }
            };
            if let Some(u) = resp.usage {
                self.fold_usage(events, &u);
            }

            // A provider mid-stream error (Anthropic error event / read failure)
            // returns stop_reason "stream_error" carrying the message in a Text
            // block + the usage accumulated before the failure. fold_usage above
            // already billed it; now surface the error so harness/batch retry
            // (transient) and result.json reports it — but the partial cost is
            // honestly accounted in self.usage / RunError.usage (CLAUDE.md
            // "重试诚实成本"). Earlier this path bailed inside the provider,
            // dropping the usage Anthropic had already billed.
            if resp.stop_reason == "stream_error" {
                let msg = resp
                    .blocks
                    .iter()
                    .rev()
                    .find_map(|b| match b {
                        Block::Text(t) if t.starts_with("[stream error] ") => {
                            Some(t["[stream error] ".len()..].to_string())
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| "stream error".to_string());
                return Err(self.run_error(
                    anyhow::Error::msg(msg),
                    &records,
                    turns,
                    start.elapsed(),
                ));
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
            // (a Sequential tool run inline, or a Parallel one run in the
            // batch below). Boxed because `ToolOutput` carries an
            // `Option<Value>` (audit `meta`) and crossed the
            // `large_enum_variant` threshold — an alloc per tool call is
            // negligible and keeps `Slot` (one per tool_use, in a Vec) small.
            enum Slot {
                Cancelled,
                Ran(Box<ToolOutput>),
            }
            let mut slots: Vec<Slot> = (0..tool_uses.len()).map(|_| Slot::Cancelled).collect();
            // Ordinals of Parallel-capable calls deferred to the parallel batch.
            let mut parallel_slots: Vec<usize> = Vec::new();
            let mut batch_cancelled = false;

            // Inline leaf pass: run Sequential tools one at a time (avoids new
            // races on shell/file), announce each LEAF call in block order, and
            // short-circuit on cancel (later slots stay Cancelled). Routing is
            // by CAPABILITY — a tool whose `concurrency()` reports Parallel is
            // deferred to the batch below, whatever its name. Parallel calls
            // are NOT announced here — their ToolCall is emitted in the batch
            // block below, AFTER the cancel gate, so a call gated off by a
            // later leaf tripping cancel never dangles a ToolCall without a
            // matching ToolResult on the UI.
            for (i, (_id, name, input)) in tool_uses.iter().enumerate() {
                if batch_cancelled || self.cancelled() {
                    batch_cancelled = true;
                    continue; // leaves the default Slot::Cancelled
                }
                // Unknown names land here too (find_tool → None): run_tool
                // synthesizes the "unknown tool" error result, exactly as
                // before the capability split.
                let parallel = match find_tool(&self.tool_defs, name) {
                    Some(ti) => self.tools[ti].concurrency() == ToolConcurrency::Parallel,
                    None => false,
                };
                if parallel {
                    parallel_slots.push(i); // ToolCall announced + run in the batch below
                } else {
                    events(AgentEvent::ToolCall {
                        name: name.clone(),
                        input: input.clone(),
                    });
                    let output = {
                        let mut fwd = |ev: AgentEvent| events(ev);
                        run_tool(&self.tools, &self.tool_defs, name, input, &self.ctx, &mut fwd)
                    };
                    slots[i] = Slot::Ran(Box::new(output));
                }
            }

            // Parallel batch: every call whose tool reported
            // ToolConcurrency::Parallel runs on its own scoped thread
            // (borrowing the read-only tool list + ctx, both Sync), forwarding
            // its events through an mpsc funnel the parent drains live — so
            // concurrent sub-agents interleave on screen, tagged by name.
            // Sequential tools stay inline (above); routing is by the
            // capability marker, NOT by name, so a third-party tool opting
            // into Parallel gets the same fan-out the built-in `task` does.
            // Its implementor has signed up for reentrancy + cancel polling
            // (see ToolConcurrency::Parallel). Cancel: gate before spawning (a
            // cancel that hit during the inline pass, or one already set,
            // marks the batch cancelled and skips it); in-flight workers
            // self-terminate via the shared cancel flag they inherit,
            // returning partial/cancelled results rather than synthesised ones.
            if !parallel_slots.is_empty() && !batch_cancelled && !self.cancelled() {
                // Announce the parallel calls in block order NOW that the batch
                // is committed to run — emitting them in the inline pass (before
                // the gate) would leave a ToolCall with no matching ToolResult
                // if a later leaf tripped cancel and gated this batch off.
                for &i in &parallel_slots {
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
                    // Spawn every parallel worker BEFORE joining any, so they
                    // run concurrently. Each owns its input/name + a sender
                    // clone, and only ever touches `tx` (never `events`).
                    let mut handles: Vec<std::thread::ScopedJoinHandle<ToolOutput>> =
                        Vec::with_capacity(parallel_slots.len());
                    for &i in &parallel_slots {
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
                            .unwrap_or_else(|_| ToolOutput::error("parallel tool worker panicked"));
                        slots[parallel_slots[pos]] = Slot::Ran(Box::new(output));
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
                            meta: output.meta.clone(),
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

/// Locate a tool by name in the cached `defs`, returning its index into the
/// parallel `tools` vec. Shared by the inline leaf pass (capability routing
/// needs the live tool to read `concurrency()`) and [`run_tool`] — one linear
/// lookup, not two. Matching on `tool.def().name` instead would deep-clone
/// every probed tool's name/description/schema on EVERY call.
fn find_tool(defs: &[ToolDef], name: &str) -> Option<usize> {
    defs.iter().position(|d| d.name == name)
}

/// Look up a tool by name ([`find_tool`]) and run it through `run_with_emit`
/// (so tools with progress to stream — the `task` sub-agent — can emit into
/// the parent run). A missing tool or a tool Err becomes a `ToolOutput::error`
/// rather than aborting the run — the result is still paired N:N with the
/// tool_use that triggered it. Shared by the sequential leaf pass and the
/// parallel batch.
fn run_tool(
    tools: &[Box<dyn Tool>],
    defs: &[ToolDef],
    name: &str,
    input: &Value,
    ctx: &ToolCtx,
    emit: &mut dyn FnMut(AgentEvent),
) -> ToolOutput {
    match find_tool(defs, name) {
        Some(i) => match tools[i].run_with_emit(input, ctx, emit) {
            Ok(o) => o,
            Err(e) => ToolOutput::error(format!("{e:#}")),
        },
        None => ToolOutput::error(format!("unknown tool: {name}")),
    }
}

pub fn preview(s: &str) -> String {
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
        // The summary request's max_tokens must stay within the configured
        // turn budget: strict gateways (some OpenAI-compatible deployments)
        // reject a per-request max_tokens above the model/account cap with a
        // 400, which would fail the summary call, set compress_failed, and
        // silently disable compression for the whole run. summary_budget
        // clamps to [1024, 4096], but an eval config can set max_tokens < 1024
        // — clamp the budget down to it (with a 256 floor so a summary isn't
        // cut to nothing). 256 is the smallest useful summary length.
        let cap = self.max_tokens.max(256);

        if transcript_size <= compress::SUMMARY_CHUNK_CHARS {
            // Common case: one summary call with a proportional budget.
            let transcript = lines.concat();
            let budget = compress::summary_budget(transcript_size).min(cap);
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
                    compress::summary_budget(chunks[0].len()).min(cap),
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
                    compress::SUMMARY_CHUNK_BUDGET.min(cap),
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
            let budget = compress::summary_budget(combined.len()).min(cap);
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
            per_request_timeout: Some(match deadline {
                Some(d) => {
                    let remaining = d
                        .saturating_duration_since(Instant::now())
                        .saturating_sub(compress::COMPRESS_RESERVE);
                    remaining / (n_calls as u32)
                }
                // No run budget: the summary calls still get a per-request
                // cap (the reader-thread leak backstop), evenly shared.
                None => STREAM_BACKSTOP / (n_calls as u32),
            }),
            cancel: self.cancel_flag(),
            // The summary is one-off: the next compression summarizes a
            // different transcript, so a cache breakpoint would only ever pay
            // the 1.25× write premium with no subsequent read. Skip caching.
            cache: false,
            // Summaries don't reason — drop the thinking config so a reasoning
            // model doesn't burn a thinking trace on a compression call.
            reasoning_effort: None,
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
