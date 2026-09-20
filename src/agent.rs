use crate::compress;
use crate::config::{Config, Limits, McpServerConfig};
use crate::hooks::ToolDecision;
use crate::provider::{Block, ChatEvent, ChatRequest, Message, Provider, Role, ToolDef, Usage};
use crate::tools::{BudgetGate, Tool, ToolConcurrency, ToolCtx, ToolOutput};
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
/// crossing the configured threshold, overflow recovery after the provider
/// rejected a request as too long for the context window, or the host
/// explicitly asking for one between runs ([`Agent::compact`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum CompressTrigger {
    Preflight,
    Overflow,
    Manual,
}

/// What a compression pass reports to its caller. Public shape for the
/// host-initiated [`Agent::compact`]; the run loop consumes the same struct
/// internally (billing only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct CompressOutcome {
    /// Middle messages folded into the summary (`cut - 1`). `0` together with
    /// `usage: Some(_)` marks the degenerate pass: the summary call ran (and
    /// billed) but its text was empty/unusable, so nothing was folded and
    /// compression is disabled for the rest of the run (a `Warning` said why).
    pub dropped: usize,
    /// History length after the fold (`messages().len()` at the event).
    pub kept: usize,
    /// Usage billed by the summary call(s); `None` = provider reported none.
    pub usage: Option<Usage>,
}

/// A compression that FAILED but still burned tokens — the Err twin of
/// [`CompressOutcome::usage`]. `summary_call` attaches every token its
/// requests reported before dying (a mid-stream stop_reason failure keeps its
/// merged `spent`; a provider `RunError` carries its own partial usage), so
/// the caller can bill honestly instead of dropping the cost on the floor
/// (the same discipline that folds failed attempts into `result.json`). The
/// run loop folds `burned` into its totals via `bill_chat`; the host-facing
/// `compact` surfaces the error as-is (Display delegates to `source`, so
/// existing `{e}` formatting is unchanged) — the host has no ledger to bill,
/// but the usage rides the anyhow chain for a binding to surface later.
#[derive(Debug)]
pub struct CompressError {
    pub source: anyhow::Error,
    /// Tokens burned by the summary request(s) before the failure.
    pub burned: Option<Usage>,
}

impl std::fmt::Display for CompressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.source, f)
    }
}
impl std::error::Error for CompressError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
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
    /// A `console.log` line from inside a `run_code` sandbox (code feature) —
    /// serializes as `{"CodeLog": "..."}`. Emitted while the host-side log
    /// buffer is under its cap (§6: events stop with collection once full).
    /// Harness `--json` maps it to `{"type":"code_log","delta":…}`; plain
    /// headless prints to stderr (ThinkingDelta discipline); the TUI commits
    /// it as a transcript row (reviewable, `/export`-able). Never enters
    /// history — the collected logs ride the tool result instead.
    CodeLog(String),
    /// Incremental fragment of a tool call's input JSON while the model
    /// streams it — serializes as `{"ToolInputDelta": {"id":…, "name":…,
    /// "delta":…}}`. Lets a UI render "constructing call…" with progressively
    /// assembled arguments instead of a black box until the call completes
    /// (`id`/`name` pair fragments across parallel same-name calls). Display
    /// feed only: the authoritative assembled input arrives in the `ToolCall`
    /// event / `Block::ToolUse`; this never enters history.
    ToolInputDelta {
        id: String,
        name: String,
        delta: String,
    },
    ToolCall {
        name: String,
        input: Value,
    },
    ToolResult {
        name: String,
        is_error: bool,
        content: String,
    },
    /// The agent updated its self-tracked task plan via the `todo` tool (full
    /// replacement each call). Emitted by the `todo` tool itself, so `--json`
    /// pipelines can render live progress; serializes as `{"TodoUpdate":
    /// {"todos":[...]}}`. The final plan is also derived into `result.json`'s
    /// `plan` field at run end.
    TodoUpdate {
        todos: Vec<TodoItem>,
    },
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
    /// Stale oversized tool results were replaced with compact placeholders —
    /// the pre-LLM context gate (no request billed). `messages` counts
    /// messages that lost content, `chars_freed` the approximate reclaimed
    /// size. tool_use/tool_result pairing is preserved; the model re-runs a
    /// tool if it needs pruned details back.
    Pruned {
        messages: usize,
        chars_freed: usize,
    },
    /// The reply hit the output-token limit (`stop_reason: "max_tokens"`)
    /// and the agent appended its one-shot wrap-up turn
    /// (`Config::salvage_on_max_tokens`, default on) to salvage a usable
    /// conclusion — at most once per run, billed as an ordinary turn.
    /// Serializes as the string `"MaxTokensSalvage"` (unit variant, external
    /// tagging); harness `--json` maps it to `{"type":"salvage"}` and plain
    /// headless writes one stderr line. Informational — the wrap-up request
    /// itself is an ordinary main turn (hooks / billing / turn budget all
    /// apply to it like any other).
    MaxTokensSalvage,
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
    SubAgent {
        name: String,
        event: Box<AgentEvent>,
    },
    Done {
        stop_reason: String,
    },
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

impl RunError {
    /// True when the run died on a provider "request too long for the context
    /// window" rejection that the overflow escape already failed to fix (or
    /// couldn't attempt). DETERMINISTIC: a retry re-sends an at-best
    /// equally-long request and fails identically — `run_with_retries` and
    /// batch use this to skip the retry ladder and to label the failure
    /// `context_overflow` (distinct from transient `error`).
    pub fn is_context_overflow(&self) -> bool {
        crate::compress::is_context_overflow(&self.source)
    }

    /// Coarse retry taxonomy (external re-review P2), shared by the
    /// single-run ladder and batch: false for DETERMINISTIC request
    /// rejections a resent request cannot fix — auth/permission (401/403),
    /// not-found (404), unfixable body (400/422) — true for transient
    /// failures worth the backoff ladder: rate limit (429), 5xx, network
    /// connect/timeout/reset, and anything unrecognized (the pre-taxonomy
    /// behavior, kept as the safe default for custom providers).
    ///
    /// Context overflow is judged separately ([`Self::is_context_overflow`])
    /// — it has its own gate and label in both retry drivers.
    pub fn is_transient(&self) -> bool {
        for cause in self.source.chain() {
            if let Some(se) = cause.downcast_ref::<crate::provider::ApiStatusError>() {
                return se.status == 429 || se.status >= 500;
            }
        }
        true
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
        // Saturating on every numeric fold: a provider reporting absurd
        // totals must not panic (debug) / wrap (release) the accumulator.
        self.usage.input_tokens = self
            .usage
            .input_tokens
            .saturating_add(re.usage.input_tokens);
        self.usage.output_tokens = self
            .usage
            .output_tokens
            .saturating_add(re.usage.output_tokens);
        self.usage.cache_creation_input_tokens = self
            .usage
            .cache_creation_input_tokens
            .saturating_add(re.usage.cache_creation_input_tokens);
        self.usage.cache_read_input_tokens = self
            .usage
            .cache_read_input_tokens
            .saturating_add(re.usage.cache_read_input_tokens);
        self.tool_calls.extend(re.tool_calls.iter().cloned());
        self.turns = self.turns.saturating_add(re.turns);
        self.elapsed = self.elapsed.saturating_add(re.elapsed);
    }

    /// Fold this accumulated cost into a successful outcome so its reported
    /// totals cover EVERY attempt. Tool calls from earlier (failed) attempts
    /// come first — they happened first chronologically.
    pub(crate) fn fold_into(&mut self, o: &mut RunOutcome) {
        // Saturating — see add_failed.
        o.input_tokens = o.input_tokens.saturating_add(self.usage.input_tokens);
        o.output_tokens = o.output_tokens.saturating_add(self.usage.output_tokens);
        o.cache_creation_input_tokens = o
            .cache_creation_input_tokens
            .saturating_add(self.usage.cache_creation_input_tokens);
        o.cache_read_input_tokens = o
            .cache_read_input_tokens
            .saturating_add(self.usage.cache_read_input_tokens);
        // Earlier attempts first, then this attempt's tool calls.
        self.tool_calls.append(&mut o.tool_calls);
        o.tool_calls = std::mem::take(&mut self.tool_calls);
        o.turns = o.turns.saturating_add(self.turns);
        o.elapsed = o.elapsed.saturating_add(self.elapsed);
    }
}

/// One-shot wrap-up nudges for `salvage_on_max_tokens`. Deliberately generic
/// English (aligned with `SUBAGENT_SYSTEM_PROMPT`) and deliberately NOT
/// phrased around any evaluation's answer-format contract — the product must
/// not couple to a benchmark's "Final answer:" convention.
const SALVAGE_NUDGE_PARTIAL: &str = "Your previous reply was cut off at the \
     output-token limit. Do not continue it — finish now with a concise, \
     complete statement of the result.";
/// Variant for a reply that died with ZERO blocks (reasoning consumed the
/// whole budget; the empty assistant message never entered history). The
/// nudge folds into the trailing user turn via `append_user_turn`.
const SALVAGE_NUDGE_EMPTY: &str = "Your previous reply produced no visible \
     text before hitting the output-token limit (reasoning consumed the \
     budget). Reply now with minimal reasoning and a direct, concise answer.";

/// 打转护栏（OpenHands Stuck Detector 的思路，裁剪到 ReAct 循环）：同一个
/// （工具名 + 规范化**生效**参数 + **结果指纹**）连续出现到
/// [`STUCK_NUDGE_AT`] 次，把纠偏提示追加进该次的 tool_result 历史块（UI 事件
/// 与 result.json 的 records 保持原样），给模型一次自愈机会；轮次收尾时连续
/// 计数仍 ≥ [`STUCK_STOP_AT`]，则以 `stop_reason = "stuck_loop"` 收尾。
/// **进展感知**（2026-09 外审第四轮 P2）：键里带结果指纹——同一调用只要
/// 结果不同（状态轮询 10%→60%）就算有进展、计数重置，只有"同参同果"才是
/// 原地打转；键用 hook Modify 后的**生效参数**而非原始参数——审批层改写
/// 成分页/不同目标的调用不复用同一键。已知盲区（刻意接受）：结果里嵌时间戳
/// /随机 id 的真打转会逃逸检测（宁漏勿滥，误停比漏停贵）；image 差异不进键
/// （结果指纹只看文本 content + is_error）。任何一次不同键的调用都会重置
/// 计数——换工具 / 换参数 / 结果有进展都算，因此健康的重试节奏
/// （edit → test → edit）永不误伤。轮数帽只数次数，分不清「慢而稳」和
/// 「原地打转」；轮帽放松（GUI 交互会话放到千级）后，这道护栏才是防失控
/// 烧钱的主力（Codex CLI 无打转保护出现过 9 小时压缩死循环）。
const STUCK_NUDGE_AT: u32 = 3;
const STUCK_STOP_AT: u32 = 6;

/// 纠偏提示（对模型的一次性警告，随第 N 次重复的 tool_result 下发）。
fn stuck_nudge(n: u32) -> String {
    format!(
        "\n\n[system notice] This identical tool call (same name and arguments) \
         has now run {n} times in a row. Repeating it cannot produce a different \
         result. Change the approach: adjust the arguments, pick a different \
         tool, or stop and explain the situation to the user."
    )
}

/// Cap on the session archive (messages folded out of the live history by
/// compression/pruning). Big enough for a very long session's paper trail;
/// beyond it the OLDEST archived messages drop — the archive is an undo/audit
/// aid, not an infinite log, and unbounded base64 images would bloat the
/// session file without bound.
const ARCHIVE_MAX_MESSAGES: usize = 4096;

/// A real provider measurement the preflight estimate anchors to: the prompt
/// size of the last main-chat request (input + cache-write + cache-read —
/// all three axes made up the prompt the provider saw) and the history it
/// was measured against. Private — an internal calibration, not state a host
/// reads.
struct UsageAnchor {
    prompt_tokens: u64,
    /// `messages.len()` at request time. Validity check at the next preflight:
    /// the history must have only GROWN since (appends). If the prefix changed
    /// at all (normalize merge, in-place edit, prune/fold), the anchor is void
    /// and the estimate falls back to the whole-history heuristic.
    msg_count: usize,
    /// `total_chars` of `messages[..msg_count]` at request time — catches
    /// in-place growth (e.g. `push_user` merging blocks into the trailing
    /// user message) that leaves the message COUNT unchanged.
    prefix_chars: usize,
    /// `system.as_ref().map(|s| s.len())` at request time. The anchor
    /// deliberately SURVIVES run boundaries (TUI follow-ups are where it
    /// pays), so a hot-swapped system prompt must void it instead.
    system_chars: Option<usize>,
    /// `tool_schema_chars` at request time — same rationale as `system_chars`
    /// (MCP hot add/remove rebuilds the tool table between runs).
    tool_schema_chars: usize,
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
    /// Whether a reply truncated at the output-token limit with no pending
    /// tool calls gets the one-shot wrap-up turn (from
    /// `Config::salvage_on_max_tokens`).
    salvage_on_max_tokens: bool,
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
    /// Monotonic source for agent-minted message ids (`m<n>`). Survives
    /// `clear_messages` on purpose — ids must never repeat within a session
    /// lineage, even after the transcript is dropped. Seeded past any ids in
    /// the initial history (see `fresh_message_id`) so a resumed process
    /// can't re-issue an id the saved history already uses.
    next_msg_id: u64,
    /// Running token totals for the current run (reset at run_impl entry).
    /// Kept on self — not a local — so `run_error` can snapshot the partial
    /// cost into the returned `RunError` when a run fails mid-way, letting a
    /// retry loop bill the failed attempt's burned tokens (honest cost).
    usage: Usage,
    /// Messages folded out of the live history by compression/pruning, kept
    /// for host-side undo/audit and persisted as the session file's `archive`
    /// section (OpenCode's "hide, don't delete"). FIFO-capped at
    /// [`ARCHIVE_MAX_MESSAGES`]. `/clear` keeps it — clearing starts a new
    /// conversation, it does not burn the old one's paper trail.
    session_archive: Vec<Message>,
    /// Real prompt size the provider reported for the last main-chat request,
    /// plus what it was measured against — the preflight compaction estimate
    /// anchors to it instead of the whole-history chars-per-token heuristic
    /// (which under-counts CJK 2–3× and would delay compaction until the
    /// provider rejects the request). `None` until the first billed response;
    /// voided whenever the measured history/prompt shape no longer matches
    /// (prune/fold rewrite, system or tool-table hot-swap, prefix edit).
    usage_anchor: Option<UsageAnchor>,
    /// Pre-serialized size of the tool definitions (name + description +
    /// schema JSON). Fixed at construction except for MCP hot add/remove,
    /// which rebuilds it via `refresh_tool_defs` — recomputing per turn would
    /// re-serialize every schema on EVERY turn's preflight check.
    tool_schema_chars: usize,
    /// The ONE top-level `run_code` instance (design §8.6, H3): constructed by
    /// `assemble` and installed post-construction via `set_code_tool`. When
    /// enabled, a CLONE lives in `tools` — both share the
    /// `Arc<RwLock<CodeConfig>>`, so `set_code_config` reaches the instance
    /// the model actually dispatches to (that sharing is the whole point of
    /// the single-construction-point rule). `None` on feature-off builds and
    /// for agents built by direct `try_new` (callers who want the tool must
    /// go through the `build_agent*` helpers).
    #[cfg(feature = "code")]
    code_tool: Option<crate::tools::run_code::RunCodeTool>,
    /// Assembly-time audit facts `(skill_injection, precheck)` for the harness's
    /// result.json / NDJSON anchor (finetune_support NFR-3). Captured at
    /// install time — NOT read live — because `skill_injection` is an
    /// assembly-time decision: a runtime `set_code_config` replace does not
    /// re-inject (FD-7), so the live value could misreport what actually
    /// happened. `Some` only when the sandbox was enabled at assembly;
    /// `None` on feature-off builds and direct `try_new` agents.
    #[cfg(feature = "code")]
    code_audit: Option<(crate::config::SkillInjection, bool)>,
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
    let env =
        format!("# Environment\n- OS: {os}\n- Shell: {shell}\n- Working directory: {workdir}");
    Some(match base {
        Some(b) if !b.is_empty() => format!("{env}\n\n{b}"),
        _ => env,
    })
}

impl Agent {
    /// Construct an [`Agent`], panicking on an invalid tool set (duplicate,
    /// empty, illegal-charset, or over-length names).
    ///
    /// The panic is a construction-time programmer error, never a runtime
    /// failure path — for a recoverable error (e.g. names come from user
    /// input in a host-language binding), use [`Agent::try_new`]. The
    /// `build_agent*` helpers already route through `try_new`, so this
    /// contract only bites hand-built tool lists.
    pub fn new(
        cfg: &Config,
        provider: Box<dyn Provider>,
        tools: Vec<Box<dyn Tool>>,
        initial: Vec<Message>,
        ctx: ToolCtx,
    ) -> Self {
        Self::try_new(cfg, provider, tools, initial, ctx).unwrap_or_else(|e| {
            panic!(
                "invalid tool set: {e:#}; construct via Agent::try_new to handle \
                 this as a recoverable error"
            )
        })
    }

    /// [`Agent::new`] as a `Result`: same construction, but tool-name
    /// validation failure is returned instead of panicking. Every entry point
    /// funnels through here — `build_agent*` (after assembly adds built-in /
    /// MCP / skill tools) and direct construction — so no path can build an
    /// agent whose dispatch silently shadows a duplicate name or whose names
    /// only fail later as a provider 400.
    pub fn try_new(
        cfg: &Config,
        provider: Box<dyn Provider>,
        tools: Vec<Box<dyn Tool>>,
        initial: Vec<Message>,
        ctx: ToolCtx,
    ) -> Result<Self> {
        crate::tools::validate_tool_names(&tools)?;
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
        // Ingest point for the initial history: id-less messages (a fresh
        // task, an old-format session file) get agent-minted ids here; ones
        // that already carry an id (resumed save, host-supplied) keep it.
        let mut messages = initial;
        let mut next_msg_id = 0u64;
        assign_missing_ids(&mut messages, &mut next_msg_id);
        Ok(Self {
            provider,
            provider_label: cfg.provider.as_str().to_string(),
            tools,
            tool_defs,
            messages,
            system,
            model: cfg.model.clone(),
            max_tokens: cfg.max_tokens,
            reasoning_effort: cfg.reasoning_effort.clone(),
            limits: cfg.limits.clone(),
            salvage_on_max_tokens: cfg.salvage_on_max_tokens,
            ctx,
            session_path: None,
            cancel: None,
            shutdown: None,
            compress_failed: false,
            next_msg_id,
            usage: Usage::default(),
            usage_anchor: None,
            session_archive: Vec::new(),
            tool_schema_chars,
            #[cfg(feature = "code")]
            code_tool: None,
            #[cfg(feature = "code")]
            code_audit: None,
        })
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

    // ── Host hot-update setters (long-lived agents) ──────────────────────────
    //
    // For CLI runs the config is fixed at build time, but a GUI host keeps
    // one Agent alive for a whole session and wants to swap request knobs
    // WITHOUT the rebuild (an MCP handshake re-run costs seconds — the reason
    // hosts asked for these). All of them are pure field replacements that
    // take effect on the NEXT request: the model/effort/budget travel on
    // `ChatRequest` per call and are never baked into the provider or the
    // tool assembly. The `&mut self` signature is the sequencing contract —
    // a run holds the borrow, so "call between runs" is enforced by the
    // compiler, not convention.

    /// Swap the model for subsequent requests (same provider/endpoint).
    ///
    /// Scope notes:
    /// - `task` sub-agents spawned later still use the model captured at
    ///   construction — they do NOT follow this setter (the `task` tool holds
    ///   a config snapshot). Swap before spawning if it must apply.
    /// - With a session enabled, the next save records the new model — a
    ///   resumed session keeps the swap.
    /// - Validity is the caller's: an unknown model name fails at the next
    ///   request, honestly, like a config typo.
    pub fn set_model(&mut self, model: &str) {
        self.model = model.to_string();
    }

    /// Swap the reasoning-effort hint forwarded to the provider verbatim
    /// (`None` = server default). Same next-request semantics as
    /// [`Agent::set_model`]; as everywhere, no enum validation — vendor
    /// ladders differ and shift across generations.
    pub fn set_reasoning_effort(&mut self, effort: Option<&str>) {
        self.reasoning_effort = effort.map(str::to_string);
    }

    /// Swap the per-request output budget (`max_tokens`). Clamped to ≥1: a
    /// persistent 0 would turn every subsequent request into an instant
    /// provider 400, and "generate nothing" is not a meaningful persistent
    /// setting. Same next-request semantics as [`Agent::set_model`].
    pub fn set_max_tokens(&mut self, max_tokens: u32) {
        self.max_tokens = max_tokens.max(1);
    }

    /// Update the context-window ceiling used by the preflight compression
    /// estimate (`limits.max_context_tokens`; 0 = fall back to the char
    /// heuristic). The config value is static by design, but a host learns
    /// the REAL window from its model catalog at runtime — a wrong ceiling
    /// makes the preflight fire far too early or never (small-window models
    /// then lean entirely on overflow recovery, which wastes a full request
    /// before it triggers). Affects the next preflight check only; no other
    /// limit is touched and `max_total_tokens` is unaffected.
    pub fn set_max_context_tokens(&mut self, tokens: u64) {
        self.limits.max_context_tokens = tokens;
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
    ///
    /// Contract: the flag is level-triggered, owned by the embedder, and never
    /// cleared by [`Agent::run`] — clear a stale stop at your acceptance point
    /// with [`Agent::clear_cancel`].
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

    // ── MCP hot add/remove (long-lived agents) ───────────────────────────────
    //
    // Startup fixes the MCP server set from the config; a host keeping one
    // Agent alive for a whole session instead wants to attach a server the
    // user just configured (and drop one they switched off) WITHOUT a full
    // rebuild — a rebuild re-runs every surviving server's handshake. Same
    // `&mut self` sequencing contract as the hot-update setters above: a run
    // holds the borrow, so "call between runs" is compiler-enforced.

    /// Display names of every tool currently exposed (built-in, MCP,
    /// skill-injected, host-registered). Lets a host confirm what a hot-add
    /// landed as, or which names a removal took away.
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_defs.iter().map(|d| d.name.clone()).collect()
    }

    /// Attach one MCP server at runtime: spawn the child (stdio) or open the
    /// HTTP transport, run the handshake, and add the tools to the NEXT
    /// request. Returns the new display names (`mcp_<server>_<tool>`).
    ///
    /// - A name collision with any existing tool (or within the new server
    ///   itself) is a recoverable [`anyhow::Error`]: the just-spawned child is
    ///   killed by dropping the fresh tool set, and the agent is left exactly
    ///   as it was. No partial add.
    /// - This never prints (unlike startup's `mcp: … ready` line): a host can
    ///   drive a raw-mode TUI, where a stray eprintln corrupts the screen.
    /// - NOT propagated to `task` sub-agents: the `task` tool snapshots the
    ///   MCP tool set at construction, so a server added here never reaches a
    ///   child spawned at any point in this agent's lifetime (sharing that
    ///   slot is deliberately deferred).
    /// - If the session-end shutdown flag is already armed (see
    ///   [`Agent::set_shutdown`]), the new tools arm it too — a server
    ///   hot-added shortly before quit isn't orphaned.
    pub fn add_mcp_server(&mut self, cfg: McpServerConfig) -> Result<Vec<String>> {
        let existing = self.tool_names();
        let fresh = crate::tools::mcp::build_one(&cfg, &existing)?;
        let mut added = Vec::with_capacity(fresh.len());
        for mut t in fresh {
            if let Some(flag) = &self.shutdown {
                t.set_shutdown(flag);
            }
            added.push(t.def().name.clone());
            self.tools.push(Box::new(t));
        }
        self.refresh_tool_defs();
        Ok(added)
    }

    /// Detach an MCP server by CONFIG name (the `name` field of
    /// [`McpServerConfig`] — NOT the sanitized `mcp_<server>_<tool>` display
    /// names). Removes every tool that server contributed; the connection's
    /// stdio child dies when the LAST holder drops — immediately if only this
    /// agent held it, at run end if an in-flight `task` sub-agent still holds
    /// a snapshot clone (the armed shutdown reaper also covers it at
    /// session end).
    ///
    /// Returns how many tools were removed; errors when the agent has no
    /// tools from a server by that name (typo, or never added) so a silent
    /// no-op can't masquerade as success.
    pub fn remove_mcp_server(&mut self, name: &str) -> Result<usize> {
        let before = self.tools.len();
        self.tools.retain(|t| t.mcp_server() != Some(name));
        let removed = before - self.tools.len();
        if removed == 0 {
            anyhow::bail!("no MCP server '{name}' attached to this agent");
        }
        self.refresh_tool_defs();
        Ok(removed)
    }

    /// Rebuild the cached request-side tool mirror (`tool_defs` + the
    /// schema-char estimate) after the tool list changed at runtime.
    /// `validate_tool_names` doesn't re-run: hot-added names are structurally
    /// valid (`sanitize_name` emits only `[A-Za-z0-9_]`, capped at 64) and
    /// unique by construction (`build_one`'s collision gate), so the
    /// `try_new` invariant holds.
    fn refresh_tool_defs(&mut self) {
        self.tool_defs = self.tools.iter().map(|t| t.def()).collect();
        self.tool_schema_chars = self
            .tool_defs
            .iter()
            .map(|t| t.name.len() + t.description.len() + t.schema.to_string().len())
            .sum();
    }

    /// Install the third-party interception layer ([`crate::hooks::Hooks`]).
    /// Post-construction setter (same pattern as `set_cancel`): assembly leaves
    /// `ToolCtx::hooks` None, so the binary and bindings behave exactly as
    /// before until a library user opts in. `task` sub-agents inherit the layer
    /// automatically (their ctx clones this Arc — see `tools::subagent::child_ctx`),
    /// so a policy installed here gates the WHOLE run, children included.
    /// An agent rebuilt from `--resume` needs `set_hooks` again — the layer
    /// lives on the instance, not in the session file.
    pub fn set_hooks(&mut self, hooks: Arc<dyn crate::hooks::Hooks>) {
        self.ctx.hooks = Some(hooks);
    }

    // ── run_code switch face (code feature, design §8.6) ─────────────────────

    /// Install the single top-level `run_code` instance ([`crate::assemble`]
    /// is the only caller — the single-construction-point rule, H3). Same
    /// post-construction-setter shape as [`Agent::set_hooks`]: `try_new`'s
    /// signature stays feature-neutral. The instance's enablement is already
    /// reflected in the tool list assemble handed to `try_new`; this only
    /// parks the live handle the switch methods below act on.
    #[cfg(feature = "code")]
    pub fn set_code_tool(&mut self, tool: crate::tools::run_code::RunCodeTool) {
        self.code_tool = Some(tool);
    }

    /// Install the assembly-time audit facts (see the `code_audit` field doc).
    /// Called by `CodeInstall::install` — the only path that knows the
    /// effective post-merge config the session assembled with.
    #[cfg(feature = "code")]
    pub(crate) fn set_code_audit(&mut self, audit: Option<(crate::config::SkillInjection, bool)>) {
        self.code_audit = audit;
    }

    /// The assembly-time code audit facts `(skill_injection, precheck)`, for
    /// the harness's result.json / NDJSON anchor. `None` when the sandbox was
    /// not enabled at assembly (or no assembly happened).
    #[cfg(feature = "code")]
    pub(crate) fn code_audit(&self) -> Option<(crate::config::SkillInjection, bool)> {
        self.code_audit
    }

    /// Atomically replace the WHOLE [`CodeConfig`] (switch + every limit /
    /// path knob). The write lands in the `Arc<RwLock<CodeConfig>>` shared
    /// with the boxed instance in the tool list, so it governs the NEXT
    /// execution — in-flight executions keep the snapshot they started with
    /// (read-lock-clone-release inside `RunCodeTool`). An `enabled` flip
    /// syncs the tool table: enabling pushes a clone of the installed
    /// instance (same shared Arc — A-10), disabling removes it.
    ///
    /// Errors when this agent has no installed instance (feature-off build,
    /// or direct `try_new` construction) — there is nothing to configure.
    #[cfg(feature = "code")]
    pub fn set_code_config(&mut self, cfg: crate::config::CodeConfig) -> Result<()> {
        let tool = self
            .code_tool
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no run_code tool installed on this agent"))?;
        let want = cfg.enabled;
        {
            let mut g = tool
                .code_cfg
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *g = cfg;
        }
        self.sync_code_tool(want)
    }

    /// Flip just the `enabled` bit (convenience derivative of
    /// [`Agent::set_code_config`]). Enabling makes `run_code` visible to the
    /// model on the NEXT request (`refresh_tool_defs` rebuilds the mirror);
    /// disabling removes it from both dispatch and the request payload —
    /// zero schema cost while off (D15).
    #[cfg(feature = "code")]
    pub fn set_code_enabled(&mut self, enabled: bool) -> Result<()> {
        let tool = self
            .code_tool
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no run_code tool installed on this agent"))?;
        {
            let mut g = tool
                .code_cfg
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.enabled = enabled;
        }
        self.sync_code_tool(enabled)
    }

    /// Make the tool table agree with the config's `enabled` bit.
    /// Push/remove + `refresh_tool_defs` — the same rebuild path MCP hot
    /// add/remove uses. The pushed instance is a CLONE of the installed one:
    /// cheap (snapshot fields + a shared registry Arc), and the shared
    /// `Arc<RwLock<CodeConfig>>` is what keeps later `set_code_config`
    /// calls reaching it.
    #[cfg(feature = "code")]
    fn sync_code_tool(&mut self, want: bool) -> Result<()> {
        let has = self.tool_defs.iter().any(|d| d.name == "run_code");
        if want == has {
            return Ok(());
        }
        if want {
            let Some(tool) = self.code_tool.as_ref() else {
                anyhow::bail!("no run_code tool installed on this agent");
            };
            // has == false here (want != has held), so the push cannot
            // collide. The one corner: a HAND-built agent (direct `try_new`
            // + `set_code_tool`) whose table already carries a same-named
            // custom tool took the early return above — that tool stays and
            // is treated as the run_code face; assembly-built agents can't
            // reach this (the whitelist strip guarantees uniqueness). The
            // remove branch below mirrors the same hand-built corner in
            // reverse: `retain` drops a same-named custom tool too. Both
            // corners are out of contract for hand-built agents — assembly
            // agents never see them.
            self.tools.push(Box::new(tool.clone()));
        } else {
            self.tools.retain(|t| t.def().name != "run_code");
        }
        self.refresh_tool_defs();
        Ok(())
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
        // Saturating — the read side (Usage::total) already saturates; a
        // lying/huge provider report must not panic (debug) or wrap to a
        // small total (release) here, silently hiding the budget breach.
        self.usage.input_tokens = self.usage.input_tokens.saturating_add(u.input_tokens);
        self.usage.output_tokens = self.usage.output_tokens.saturating_add(u.output_tokens);
        self.usage.cache_creation_input_tokens = self
            .usage
            .cache_creation_input_tokens
            .saturating_add(u.cache_creation_input_tokens);
        self.usage.cache_read_input_tokens = self
            .usage
            .cache_read_input_tokens
            .saturating_add(u.cache_read_input_tokens);
        events(AgentEvent::Usage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_creation_input_tokens: u.cache_creation_input_tokens,
            cache_read_input_tokens: u.cache_read_input_tokens,
        });
    }

    /// Share one chat's spend into the run-wide counter (`ToolCtx::budget` —
    /// see the gate in `run_impl`). The counter half of [`Self::bill_chat`],
    /// split out so the tool-billed fold can share WITHOUT double-adding a
    /// `task` child's already-shared spend.
    fn share_budget(&self, total: u64) {
        if let Some(g) = &self.ctx.budget {
            g.share(total);
        }
    }

    /// Bill one of THIS agent's own chat requests: share the spend into the
    /// run-wide counter (`ToolCtx::budget` — see the gate in `run_impl`), then
    /// fold into the local totals + emit. Used by the three direct-chat sites
    /// (main turn chat, preflight + forced-compression summaries). NOT
    /// wholesale for tool-billed usage: a `task` child already shared its own
    /// direct spend while running, so the parent's accounting fold must stay
    /// counter-free for it (a double add would halve the fleet's effective
    /// budget) — see the [`ToolOutput::usage_in_shared_budget`] split at the
    /// fold site.
    fn bill_chat(&mut self, events: &mut dyn FnMut(AgentEvent), u: &Usage) {
        self.share_budget(u.total());
        self.fold_usage(events, u);
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

    /// Restore a session archive previously read via
    /// [`session::load_with_archive`], so later saves keep appending to it —
    /// resuming and compacting again would otherwise write a file whose
    /// archive holds only the new fold's messages. No-op for hosts that don't
    /// consume the archive.
    pub fn set_session_archive(&mut self, archive: Vec<Message>) {
        self.session_archive = archive;
        // Keep the mint counter ahead of the restored ids even though no
        // archive_messages call happened in this process (see its doc).
        self.bump_id_counter_past_archive();
    }

    /// Persist the session, returning a warning message on failure instead of
    /// printing — the agent must never write to stderr itself (in TUI mode the
    /// worker thread shares the terminal with ratatui). Callers with an event
    /// channel forward it as `AgentEvent::Warning`.
    fn save_session(&self) -> Option<String> {
        if let Some(path) = &self.session_path {
            if let Err(e) = crate::session::save_with_archive(
                path,
                &self.model,
                &self.messages,
                &self.session_archive,
            ) {
                return Some(format!("session save failed: {e:#}"));
            }
        }
        None
    }

    /// Append a user message (multi-turn / TUI follow-ups). Returns a warning
    /// if the session file couldn't be persisted.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub fn push_user(&mut self, blocks: Vec<Block>) -> Option<String> {
        let before = self.messages.len();
        append_user_turn(&mut self.messages, blocks);
        // A NEW user message entered the history (vs. blocks merged into a
        // trailing user turn, which keeps that turn's id): mint its anchor id.
        if self.messages.len() > before && self.messages.last().unwrap().id.is_none() {
            let id = fresh_message_id(&self.messages, &mut self.next_msg_id);
            self.messages.last_mut().unwrap().id = Some(id);
        }
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
    ///
    /// NEVER clears the cancel flag (TinyWorker contract, v0.3.1): cancel is
    /// level-triggered, owned by the embedder, and a `true` observed at ANY
    /// point — including before this call — is honoured. A run started with
    /// cancel already set returns immediately from the loop-head check:
    /// `stop_reason == "cancelled"`, zero turns, zero provider requests.
    /// The party that ACCEPTS a new task (the TUI's input handler, `compact`
    /// at its entry, an embedding host's send-message path) is responsible
    /// for clearing a stale cancel at acceptance time via
    /// [`Agent::clear_cancel`] — the acceptor is the only one who knows the
    /// previous run truly ended, and its acceptance point may sit well before
    /// this call (an embedder's lazy build can take seconds). A retry ladder
    /// is deliberately NOT on this list: a cancel observed between attempts
    /// is TERMINAL for the ladder (`EXIT_CANCELLED`, see
    /// `harness::run_with_retries`) — clearing it there would revive a run
    /// the user already vetoed.
    pub fn run(&mut self, events: &mut dyn FnMut(AgentEvent)) -> Result<RunOutcome, RunError> {
        self.run_impl(events, /* top_level */ true)
    }

    /// Drive this agent as a `task` sub-agent's child. Identical to `run`
    /// except it does NOT install a fresh run: the budget gate and deadline
    /// stay the ones the parent published, so parent + siblings + child all
    /// read/write the same numbers (a parallel `task` batch must trip the
    /// token cap collectively, and a child spawned near the deadline clamps
    /// to the parent's remaining time).
    ///
    /// Since the cancel-ownership contract change, this no longer differs
    /// from `run` on cancel either — NO entry point clears the flag. A child
    /// shares the parent's cancel `Arc`; clearing it here would drop a Ctrl-C
    /// signalled for the whole batch, and even absent that, run() clearing it
    /// would race every sibling. The child is freshly built per call, so it
    /// has no stale cancel of its own; any `true` it observes on the shared
    /// flag is a real cancel it must honour.
    pub(crate) fn run_as_subagent(
        &mut self,
        events: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome, RunError> {
        self.run_impl(events, /* top_level */ false)
    }

    /// Clear the cancel flag — the acceptance-point half of the cancel
    /// contract (see [`Agent::run`]): `run` never clears cancel, so whoever
    /// accepts a NEW task clears any stale stop left over from the previous
    /// one, at the moment of acceptance. Call this from the same place the
    /// host marks the session "running" (the TUI does it when submitting a
    /// user message; `compact` at its entry; an embedding host in its
    /// send-message path). A retry ladder does NOT clear between attempts —
    /// a cancel observed there is terminal (`EXIT_CANCELLED`), not residue:
    /// clearing it would revive a run the user vetoed (see
    /// `harness::run_with_retries`). Only meaningful between runs — during a
    /// run the flag must stay untouched so a stop request lands.
    pub fn clear_cancel(&self) {
        if let Some(c) = &self.cancel {
            c.store(false, Ordering::Relaxed);
        }
    }

    fn run_impl(
        &mut self,
        events: &mut dyn FnMut(AgentEvent),
        top_level: bool,
    ) -> Result<RunOutcome, RunError> {
        let start = Instant::now();
        // Each top-level run gets a fresh compression budget. In the TUI the
        // same Agent is reused across follow-ups; without this reset, one
        // transient compression failure would disable it for the rest of the
        // session and the history would grow until it hit the model's limit.
        self.compress_failed = false;
        // Fresh usage totals per run (this Agent may be reused across runs —
        // TUI follow-ups, harness retries). The usage anchor is deliberately
        // NOT reset here: it survives run boundaries (its validity check
        // compares against the current system prompt / tool table / history
        // prefix, so a hot-swap voids it on use, not on entry).
        self.usage = Usage::default();
        // NOTE: cancel is deliberately NOT touched here. The old entry-point
        // `store(false)` silently dropped a stop signalled during the
        // embedder's acceptance window (lazy build can sit between the
        // host's "running" mark and this call) — see run()'s doc for the
        // contract and clear_cancel for the acceptance-point remedy.
        // Overall run deadline. Each chat is capped to the time left so a
        // single stuck request can't outlive the run budget — which is
        // otherwise only enforced at turn boundaries. checked_add: a plain
        // `start + Duration` panics on clock overflow (a timeout_secs near
        // u64::MAX overflows Instant on some platforms); such a value is
        // "no deadline" for all practical purposes, so None is the honest
        // rendering, identical to timeout_secs == 0.
        let deadline = (self.limits.timeout_secs > 0)
            .then(|| start.checked_add(Duration::from_secs(self.limits.timeout_secs)))
            .flatten();
        // Publish the deadline + a fleet-wide budget counter to the tool ctx
        // so `task` children inherit both (child_ctx clones them): the shared
        // counter makes N parallel sub-agents collectively bound by
        // max_total_tokens (each child's gate reads the same spend the parent
        // and its siblings write — the old gate saw only the parent's own
        // totals at turn boundaries, so a parallel batch could burn N× the
        // budget before the join-time fold), and the deadline lets a child
        // clamp its timeout to the parent's REMAINING time. Only a top-level
        // run installs a FRESH counter (usage totals also reset per run — TUI
        // follow-ups, harness retries); run_as_subagent keeps the inherited
        // one so parent + siblings + child all read/write the same numbers.
        self.ctx.deadline = deadline;
        if top_level {
            self.ctx.budget = (self.limits.max_total_tokens > 0)
                .then(|| BudgetGate::new(self.limits.max_total_tokens));
        }
        let mut turns = 0u32;
        // One-shot allowance for `salvage_on_max_tokens`: at most one
        // wrap-up turn per run (a salvage turn that ALSO truncates is
        // returned as-is, never salvaged twice).
        let mut salvage_used = false;
        let mut final_text = String::new();
        let mut records: Vec<ToolCallRecord> = Vec::new();
        // 打转检测状态：上一个已决调用的（工具名, 规范化生效参数, 结果指纹）
        // 与其连续重复次数（见 STUCK_NUDGE_AT / STUCK_STOP_AT 的护栏说明）。
        // 没有 stuck_stop 闩锁——轮尾直接读 stuck_len，任何不同键的调用重置
        // 计数即自动解除停止条件（外审第四轮 P2：闩锁曾让同轮第 7 次换法
        // 仍被停）。
        let mut stuck_key: Option<(String, String, u64)> = None;
        let mut stuck_len: u32 = 0;
        let mut stop_reason: String;

        // Labeled: the per-turn overflow-retry loop below is itself a `loop`
        // (its `break resp` yields the ChatResponse), so exiting the TURN loop
        // from inside a chat-error arm needs an explicit target.
        'turns: loop {
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
            if self.limits.max_total_tokens > 0 {
                // Fleet spend: the shared counter when installed (this run's
                // direct chat spend of the parent AND every live task child —
                // children fetch_add as they go, so a parallel batch trips the
                // cap collectively instead of each child independently burning
                // a full budget while the parent waits at the join), else our
                // own totals (hand-built ctx in tests / budget unset). The cap
                // side is single-sourced from the gate the same way: the gate
                // was armed with the fleet's max_total_tokens, and our own
                // limits snapshot may only TIGHTEN it (a future child-scoped
                // budget) — comparing the fleet counter against the snapshot
                // instead would silently unbound the fleet the day a child's
                // snapshot diverges from the parent's cap.
                let (spent, cap) = match self.ctx.budget.as_ref() {
                    Some(g) => (
                        g.used.load(Ordering::Relaxed),
                        g.cap.min(self.limits.max_total_tokens),
                    ),
                    None => (self.usage.total(), self.limits.max_total_tokens),
                };
                if spent >= cap {
                    stop_reason = "budget_exceeded".into();
                    break;
                }
            }
            turns += 1;
            events(AgentEvent::TurnStart(turns));
            // Preflight compression: if the history is large enough that this
            // request would blow the window, summarize first. The summary
            // request is a real billed call; fold its usage into the run totals
            // so result.json / TUI report honest costs. Err = the summary call
            // itself failed (compress_failed is set inside): warn through the
            // event stream — the host-facing compact propagates its Err instead,
            // which is why compress() doesn't warn on that path itself.
            match self.compress(
                events,
                deadline,
                CompressTrigger::Preflight,
                /* instructions: */ None,
            ) {
                Ok(Some(outcome)) => {
                    if let Some(u) = outcome.usage {
                        self.bill_chat(events, &u);
                    }
                }
                Ok(None) => {}
                Err(ce) => {
                    // Honest cost: the failed compression still billed its
                    // summary request(s) — fold that into the run totals so
                    // result.json doesn't undercount (the Err twin of the
                    // Ok(Some) arm's outcome.usage billing above).
                    if let Some(u) = &ce.burned {
                        self.bill_chat(events, u);
                    }
                    events(AgentEvent::Warning(format!(
                        "context compression failed (disabled for this run): {:#}",
                        ce.source
                    )));
                }
            }

            // Overflow recovery (OpenCode semantics): if the provider rejects
            // this request as too long for the context window, force ONE
            // compression and retry the turn on the shorter history; a second
            // overflow is a real error. The request borrows &self.messages, so
            // it lives in an inner block whose end releases that borrow before
            // the (mutable) compress call in the recovery arm.
            // Overflow recovery budget: ONE LLM force-compress and —
            // independently — ONE descending-window prune backstop (see the
            // recovery arm below). Two flags, not one: the backstop is the
            // cheap second chance after the fold's retry still overflows.
            let mut overflow_recovered = false;
            let mut prune_backstop_used = false;
            // History length at request time (post-normalize) — the reference
            // point the usage anchor below is measured against. Assigned inside
            // every loop iteration before any read.
            let mut request_msg_count: usize;
            let mut resp = loop {
                let r = {
                    // Safety net: fold any adjacent same-role messages (e.g. a
                    // resumed session that ends on two user messages, or a
                    // hand-edited history) into one before sending — Anthropic
                    // rejects consecutive user messages with a 400. Idempotent,
                    // so re-running it after a compression-folded history (still
                    // strictly alternating) is a no-op.
                    normalize_adjacent(&mut self.messages);
                    request_msg_count = self.messages.len();

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
                    // Hooks observation point — MAIN-turn requests only (the
                    // compression summary call deliberately not surfaced; see
                    // the Hooks doc, "Scope"). Read-only by signature.
                    if let Some(h) = &self.ctx.hooks {
                        h.on_chat_request(&req);
                    }
                    self.provider.chat(&req, &mut |e| match e {
                        ChatEvent::TextDelta(t) => events(AgentEvent::TextDelta(t)),
                        ChatEvent::ThinkingDelta(t) => events(AgentEvent::ThinkingDelta(t)),
                        ChatEvent::ToolInputDelta { id, name, delta } => {
                            events(AgentEvent::ToolInputDelta { id, name, delta })
                        }
                    })
                }; // req dropped here → &self.messages borrow released
                   // Baseline AFTER normalize_adjacent, so a same-role merge that
                   // shrank the history isn't mistaken for a successful compression
                   // fold — this gate means "did compression actually shrink it?".
                let before = self.messages.len();
                match r {
                    Ok(resp) => break resp,
                    // Overflow recovery (OpenCode semantics): the provider
                    // rejected this request as too long for the context
                    // window. Recovery budget = one LLM force-compress, and
                    // after its retry still overflows, one descending-window
                    // prune backstop. The budget checks read `self`, so they
                    // live in the body — the match guard stays pure.
                    Err(e) if compress::is_context_overflow(&e) => {
                        if overflow_recovered && prune_backstop_used {
                            return Err(self.run_error(e, &records, turns, start.elapsed()));
                        }
                        if !overflow_recovered {
                            // Cheapest first: placeholder-prune stale tool output (no
                            // LLM call). Idempotent — a later overflow finds nothing
                            // left above the minimum and falls through to the summary
                            // path below. Retry only when the estimate now fits (or no
                            // threshold is configured, the provider being the only
                            // oracle then); otherwise the retry would just re-prove
                            // the overflow.
                            if self.try_prune(events) > 0
                                && self.over_compaction_threshold() != Some(true)
                            {
                                continue;
                            }
                            // Need a compressible head and a non-failed compressor.
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
                                // Err = the summary call failed (compress_failed
                                // set inside) — warn and fall through to the
                                // unrecoverable-overflow error below.
                                match self.compress(
                                    events,
                                    deadline,
                                    CompressTrigger::Overflow,
                                    /* instructions: */ None,
                                ) {
                                    Ok(Some(outcome)) => {
                                        if let Some(u) = outcome.usage {
                                            self.bill_chat(events, &u);
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(ce) => {
                                        // Honest cost: same as the Preflight
                                        // arm — bill the failed summary's
                                        // burned tokens before warning.
                                        if let Some(u) = &ce.burned {
                                            self.bill_chat(events, u);
                                        }
                                        events(AgentEvent::Warning(format!(
                                            "context compression failed (disabled for this run): {:#}",
                                            ce.source
                                        )));
                                    }
                                }
                                // Only retry if compression actually shrank the
                                // history; otherwise there's nothing more we can do.
                                if self.messages.len() < before {
                                    continue;
                                }
                            }
                            return Err(self.run_error(e, &records, turns, start.elapsed()));
                        }
                        // overflow_recovered && !prune_backstop_used: the FOLDED
                        // history still overflows. With a small window the kept
                        // TAIL alone can exceed it (keep_recent × a full-size
                        // tool output each) — the fold removed the middle, the
                        // tail stays, and the configured protection window
                        // guards exactly that tail. Prune under a shrinking
                        // window (half the configured one, then everything but
                        // the very last message): pairing-safe placeholders,
                        // originals archived, no LLM call — one last retry.
                        let mut freed = 0;
                        for protect in [self.limits.keep_recent_messages / 2, 1] {
                            freed = self.try_prune_protect(events, protect);
                            if freed > 0 {
                                break;
                            }
                        }
                        prune_backstop_used = true;
                        if freed > 0 {
                            continue;
                        }
                        return Err(self.run_error(e, &records, turns, start.elapsed()));
                    }
                    Err(e) => {
                        // A request that only fails once the run's wall clock
                        // is gone died because the budget ran out in practice
                        // — its per-request timeout WAS the deadline
                        // remainder. Give the run the deadline vocabulary
                        // (clean "timeout" stop_reason, exit 124, transcript
                        // intact) instead of a generic provider error that
                        // would climb the retry ladder (external re-review
                        // P2). A pre-deadline failure keeps its real error.
                        if deadline.is_some_and(|d| Instant::now() >= d) {
                            stop_reason = "timeout".into();
                            break 'turns;
                        }
                        return Err(self.run_error(e, &records, turns, start.elapsed()));
                    }
                }
            };
            if let Some(u) = &resp.usage {
                self.bill_chat(events, u);
                // Usage anchor for the next preflight estimate (see
                // `estimate_input_tokens`): the provider's own count of the
                // prompt it just processed, measured against the history as
                // sent. All three input axes made up that prompt; output
                // tokens did not.
                self.usage_anchor = Some(UsageAnchor {
                    prompt_tokens: u
                        .input_tokens
                        .saturating_add(u.cache_creation_input_tokens)
                        .saturating_add(u.cache_read_input_tokens),
                    msg_count: request_msg_count,
                    prefix_chars: compress::total_chars(&self.messages[..request_msg_count]),
                    system_chars: self.system.as_ref().map(|s| s.len()),
                    tool_schema_chars: self.tool_schema_chars,
                });
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
                // Same deadline vocabulary as the Err arm of the request loop
                // (external re-review P2): a stream that only died once the
                // run's wall clock is gone must end as a clean "timeout"
                // (exit 124, partial usage already billed above, transcript
                // intact) — NOT a retryable stream error that would replay
                // the whole task on a fresh budget. A pre-deadline stream
                // error keeps the retryable Err below.
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    stop_reason = "timeout".into();
                    break 'turns;
                }
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
                // max_tokens with ZERO visible blocks (reasoning consumed
                // the whole budget): salvage once with a wrap-up turn. The
                // empty assistant message stays OUT of history (strict APIs
                // reject it — the reason this break exists); the nudge folds
                // into the trailing user message via append_user_turn.
                if resp.stop_reason == "max_tokens"
                    && self.salvage_on_max_tokens
                    && !salvage_used
                    && !self.cancelled()
                {
                    salvage_used = true;
                    events(AgentEvent::MaxTokensSalvage);
                    if let Some(w) = self.push_user(vec![Block::Text(SALVAGE_NUDGE_EMPTY.into())]) {
                        events(AgentEvent::Warning(w));
                    }
                    continue;
                }
                break;
            }
            // Repair tool_use ids BEFORE the assistant message is persisted:
            // a gateway may omit the id (empty — results then pair
            // ambiguously) or repeat one (duplicate ids make the N:N
            // tool_use ↔ tool_result mapping ambiguous; Anthropic rejects
            // both with a 400). Normalizing here means history, the
            // `tool_uses` tuples collected below, and the tool_results
            // built from them all see one consistent id set.
            normalize_tool_use_ids(&mut resp.blocks, &self.messages);
            // Anchor id minted before the push (minting needs `&self.messages`
            // for the collision scan — impossible while the push borrows).
            let assistant_id = fresh_message_id(&self.messages, &mut self.next_msg_id);
            let mut assistant = Message::assistant(resp.blocks.clone());
            assistant.id = Some(assistant_id);
            self.messages.push(assistant);
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
                // max_tokens truncation WITHOUT pending calls: the partial
                // text above is already in history — append the one-shot
                // wrap-up nudge as a fresh user turn instead of ending the
                // run on a cut-off reply. Truncation WITH tool_use keeps
                // the old semantics (half-call dropped above, stop): a
                // wrap-up turn can't usefully resume an aborted call stream.
                if resp.stop_reason == "max_tokens"
                    && !has_tool_use
                    && self.salvage_on_max_tokens
                    && !salvage_used
                    && !self.cancelled()
                {
                    salvage_used = true;
                    events(AgentEvent::MaxTokensSalvage);
                    if let Some(w) = self.push_user(vec![Block::Text(SALVAGE_NUDGE_PARTIAL.into())])
                    {
                        events(AgentEvent::Warning(w));
                    }
                    continue;
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
            // Hook-Modify replacements, per ordinal (None = ran with the
            // original input). The stuck detector keys on the EFFECTIVE input
            // (external re-review P2): an approval layer rewriting each call to
            // a different page/target must not share one stuck key.
            let mut effective_inputs: Vec<Option<Value>> =
                (0..tool_uses.len()).map(|_| None).collect();
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
                    // One complete inline call: ToolCall event → gate → run →
                    // after-notification (see execute_tool_call). The
                    // ToolResult event is NOT emitted here — the assembly pass
                    // below owns block order, N:N pairing, and the
                    // cancelled-slot synthesis.
                    let output = {
                        let mut fwd = |ev: AgentEvent| events(ev);
                        execute_tool_call(
                            &self.tools,
                            &self.tool_defs,
                            &self.ctx,
                            name,
                            input,
                            &mut fwd,
                            &mut effective_inputs[i],
                        )
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
            // marks the batch cancelled and skips it), re-checked PER CALL
            // between the approval gates, once more before the first spawn,
            // and again at the top of every wave — a cancel landing during an
            // approval wait or between waves skips the rest (external
            // re-review P2); in-flight workers self-terminate via the shared
            // cancel flag they inherit, returning partial/cancelled results
            // rather than synthesised ones. Hooks: after the announce, each
            // call passes `before_tool` — serially, on this thread, before ANY
            // worker spawns (approval decisions never race); denied calls
            // never spawn. Announced calls that never run keep their
            // Cancelled history slot but DO get a matching ToolResult event in
            // the assembly pass (`announced_parallel`) so event consumers
            // never see an unclosed ToolCall.
            let mut announced_parallel = false;
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
                announced_parallel = true;
                // Hooks gate — SERIAL on this (parent) thread, before any
                // worker spawns, in parallel-block order (see the Hooks doc,
                // "Call order & concurrency"). Denied calls get their error
                // slot + after-notification at decision time and spawn no
                // worker; Allow/Modify land in `runnable` (Modify carrying the
                // replacement — the audit trail keeps the original input from
                // `tool_uses`).
                let mut runnable: Vec<(usize, String, Value)> =
                    Vec::with_capacity(parallel_slots.len());
                for &i in &parallel_slots {
                    // Cancel checkpoint, per call (external re-review P2): the
                    // approval gates below can each wait unboundedly long, so
                    // re-check between gates — a cancel that landed during an
                    // earlier approval ends the gating here. Slots that were
                    // never gated keep their Cancelled placeholder (N:N
                    // synthesis below).
                    if self.cancelled() {
                        break;
                    }
                    let name = &tool_uses[i].1;
                    let input = &tool_uses[i].2;
                    match gate_tool_call(&self.ctx, name, input) {
                        ToolDecision::Deny(reason) => {
                            // Same output budget as any tool error (run_tool's
                            // MCP path): a host's deny reason is model-visible
                            // is_error text and can be arbitrarily long.
                            let mut reason = reason;
                            crate::tools::truncate(&mut reason, self.ctx.max_output_chars);
                            let output = ToolOutput::error(reason);
                            notify_tool_after(&self.ctx, name, input, &output, false);
                            slots[i] = Slot::Ran(Box::new(output));
                        }
                        ToolDecision::Allow => {
                            runnable.push((i, name.clone(), input.clone()));
                        }
                        ToolDecision::Modify(v) => {
                            effective_inputs[i] = Some(v.clone());
                            runnable.push((i, name.clone(), v));
                        }
                    }
                }
                // Second checkpoint before anything spawns: a cancel that
                // landed during the LAST gate skips the whole batch. Approved
                // but unstarted calls don't stay Cancelled — the placeholder
                // loop after the waves converts them to the same recorded
                // after_tool(executed=false) shape the worker pre-body
                // checkpoint uses, so a passed gate never leaves an unmatched
                // before_tool (external re-review P3). (Cancel = stop
                // STARTING work; already-running workers self-terminate via
                // the shared flag per the Parallel contract.) Control still
                // falls through to the assembly pass below — the announced
                // calls need their synthesized results.
                let mut spawned = 0usize;
                if !self.cancelled() && !runnable.is_empty() {
                    // Waves cap concurrent workers (MAX_PARALLEL_TOOL_WORKERS):
                    // the model can emit dozens of `task` calls in one turn and
                    // one-thread-per-call would fan out unboundedly. Events funnel
                    // through a BOUNDED channel per wave (sync_channel) — a flood
                    // from a fast sub-agent blocks its emit instead of ballooning
                    // memory when the parent's consumer is slow (backpressure).
                    // Within a wave every worker still spawns before any joins, so
                    // the drain loop below interleaves their events live. Cancel is
                    // re-checked at each wave start AND inside the worker right
                    // before the tool body runs — a cancel that landed during an
                    // earlier wave must not start the next one (external
                    // re-review P2).
                    const MAX_PARALLEL_TOOL_WORKERS: usize = 8;
                    for wave in runnable.chunks(MAX_PARALLEL_TOOL_WORKERS) {
                        if self.cancelled() {
                            break; // later waves get the placeholder loop below
                        }
                        let tools = &self.tools;
                        let defs = &self.tool_defs;
                        let ctx = &self.ctx;
                        std::thread::scope(|s| {
                            // The channel is created INSIDE the scope closure so
                            // `rx` is a closure local: if the host event callback
                            // panics under panic=unwind, the drain loop's unwind
                            // drops `rx`, worker sends start failing, and scope's
                            // join-all can complete — with `rx` borrowed from
                            // outside, workers blocked on a full funnel while the
                            // join waited on them: deadlock (external re-review
                            // P2). The guard additionally trips the shared cancel
                            // flag so in-flight workers wind down fast instead of
                            // running a doomed wave to completion.
                            struct CancelOnUnwind<'a> {
                                flag: &'a Option<Arc<AtomicBool>>,
                                armed: bool,
                            }
                            impl Drop for CancelOnUnwind<'_> {
                                fn drop(&mut self) {
                                    if self.armed && std::thread::panicking() {
                                        if let Some(c) = self.flag {
                                            c.store(true, Ordering::Relaxed);
                                        }
                                    }
                                }
                            }
                            let (tx, rx) = std::sync::mpsc::sync_channel::<AgentEvent>(256);
                            let mut unwind_guard = CancelOnUnwind {
                                flag: &ctx.cancel,
                                armed: true,
                            };
                            // Spawn every wave worker BEFORE joining any, so they
                            // run concurrently. Each owns its input/name + a
                            // sender clone, and only ever touches `tx` (never
                            // `events`). A worker re-checks cancel before the
                            // tool body: approved-but-unstarted calls return
                            // None, which the join below turns into the same
                            // cancelled-error shape the sequential gate
                            // checkpoint uses.
                            let mut handles: Vec<
                                std::thread::ScopedJoinHandle<Option<ToolOutput>>,
                            > = Vec::with_capacity(wave.len());
                            for (_, name, input) in wave {
                                let name = name.clone();
                                let input = input.clone();
                                let tx = tx.clone();
                                handles.push(s.spawn(move || {
                                    let mut emit = |ev: AgentEvent| {
                                        // Blocks when the funnel is full — the
                                        // drain loop in this same scope is the
                                        // only consumer, so it always unwinds.
                                        let _ = tx.send(ev);
                                    };
                                    if ctx_cancelled(ctx) {
                                        return None;
                                    }
                                    Some(run_tool(tools, defs, &name, &input, ctx, &mut emit))
                                }));
                            }
                            drop(tx); // workers hold the last clones; rx ends when they drop
                                      // Forward child events to the parent sink as they arrive
                                      // (interleaved across parallel sub-agents, told apart by name).
                            while let Ok(ev) = rx.recv() {
                                events(ev);
                            }
                            unwind_guard.armed = false; // drain finished cleanly
                                                        // Join in spawn order (= parallel block order): notify
                                                        // hooks with the ORIGINAL input (from tool_uses, never
                                                        // the Modify replacement), then place each result at
                                                        // its ordinal.
                            for (pos, h) in handles.into_iter().enumerate() {
                                let (i, _, _) = wave[pos];
                                let output = h.join().unwrap_or_else(|_| {
                                    // Usage note (external review, known
                                    // divergence): a sub-agent that panicked
                                    // mid-run has already billed its chats
                                    // into the SHARED fleet counter, but the
                                    // ToolOutput carrying its usage died with
                                    // the thread — so result.json totals never
                                    // see it. Counter-vs-totals can disagree
                                    // for a panicked parallel child; recovering
                                    // the partial usage would need a side
                                    // channel out of the panicking thread.
                                    Some(ToolOutput::error("parallel tool worker panicked"))
                                });
                                match output {
                                    Some(output) => {
                                        notify_tool_after(
                                            ctx,
                                            &tool_uses[i].1,
                                            &tool_uses[i].2,
                                            &output,
                                            true,
                                        );
                                        slots[i] = Slot::Ran(Box::new(output));
                                    }
                                    // Cancelled before the tool body ran: same
                                    // shape as the sequential gate checkpoint —
                                    // error slot, executed=false, and (the call
                                    // was announced) a ToolResult event lands in
                                    // the assembly pass.
                                    None => {
                                        let output = ToolOutput::error("[cancelled by user]");
                                        notify_tool_after(
                                            ctx,
                                            &tool_uses[i].1,
                                            &tool_uses[i].2,
                                            &output,
                                            false,
                                        );
                                        slots[i] = Slot::Ran(Box::new(output));
                                    }
                                }
                            }
                        });
                        spawned += wave.len();
                    }
                }
                // Approved-but-never-started calls (cancel at the pre-spawn
                // checkpoint or between waves): their before_tool already
                // fired — possibly after an approval wait — so they owe the
                // audit trail the same recorded after_tool(executed=false)
                // placeholder the worker pre-body checkpoint produces. Model-
                // visible output is unchanged (the assembly pass was already
                // synthesizing this exact ToolResult for announced slots);
                // only records / hook pairing gain the entry. Never-gated
                // calls (gate-loop break) keep their Cancelled slot: no
                // before_tool fired, so there is nothing to pair.
                for &(i, ref name, _) in &runnable[spawned..] {
                    let output = ToolOutput::error("[cancelled by user]");
                    notify_tool_after(&self.ctx, name, &tool_uses[i].2, &output, false);
                    slots[i] = Slot::Ran(Box::new(output));
                }
            }

            // Assemble results in block order: a result block for every tool_use
            // (N:N), but records / ToolResult events / usage fold only for tools
            // that actually ran. A `task` call's usage (the child's honest cost)
            // is folded here exactly like a leaf tool's billed usage.
            let mut results: Vec<Block> = Vec::with_capacity(tool_uses.len());
            for (i, (id, name, input)) in tool_uses.iter().enumerate() {
                match &slots[i] {
                    Slot::Cancelled => {
                        // A cancelled call that WAS announced (parallel batch
                        // committed, then a gate/wave cancel kept it from
                        // running) owes the event stream a ToolResult — the
                        // consumer already saw its ToolCall and must not be
                        // left with an open pair (external re-review P2).
                        // Never-announced slots (inline-pass short-circuit,
                        // batch gated off) stay silent on the event side.
                        if announced_parallel {
                            events(AgentEvent::ToolResult {
                                name: name.clone(),
                                is_error: true,
                                content: "[cancelled by user]".into(),
                            });
                        }
                        results.push(Block::ToolResult {
                            tool_use_id: id.clone(),
                            content: "[cancelled by user]".into(),
                            image: None,
                            is_error: true,
                        })
                    }
                    Slot::Ran(output) => {
                        // Tool-billed usage always folds into the run totals
                        // (billing is honest regardless of source); the shared
                        // counter gets it only when the producer did NOT
                        // already share while running. `task` children set
                        // usage_in_shared_budget (their own chats billed into
                        // the inherited counter — re-adding here would halve
                        // the fleet budget); any other tool reporting usage
                        // (a host CallbackTool metering an upstream API) never
                        // entered a counter, so the fold shares it now —
                        // previously that spend billed the totals yet was
                        // invisible to the max_total_tokens gate (external
                        // review P2-3).
                        if let Some(u) = output.usage {
                            if !output.usage_in_shared_budget {
                                self.share_budget(u.total());
                            }
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
                        // 打转计数：键 =（工具名, 规范化生效参数, 结果指纹）——
                        // 同参不同果（轮询有进展）不同键、自动重置；nudge 只
                        // 追加进历史块——ToolResult 事件与 records 保持原样输出
                        let key = (
                            name.clone(),
                            canon_input(effective_inputs[i].as_ref().unwrap_or(input)),
                            stuck_result_hash(output),
                        );
                        if stuck_key.as_ref() == Some(&key) {
                            stuck_len += 1;
                        } else {
                            stuck_key = Some(key);
                            stuck_len = 1;
                        }
                        let mut content = output.content.clone();
                        if stuck_len == STUCK_NUDGE_AT {
                            content.push_str(&stuck_nudge(stuck_len));
                        }
                        results.push(Block::ToolResult {
                            tool_use_id: id.clone(),
                            content,
                            image: output.image.clone().map(Box::new),
                            is_error: output.is_error,
                        });
                    }
                }
            }
            // Anchor id minted before the push (minting scans `&self.messages`
            // for collisions — impossible while the push borrows it).
            let results_id = fresh_message_id(&self.messages, &mut self.next_msg_id);
            let mut results_msg = Message::user(results);
            results_msg.id = Some(results_id);
            self.messages.push(results_msg);
            if let Some(w) = self.save_session() {
                events(AgentEvent::Warning(w));
            }
            // 轮尾终止判定：取消优先于打转硬停（外审第四轮 P2——本轮内落下
            // 的 cancel 必须报 "cancelled"/exit 130，不能被 stuck_loop 抢走）；
            // 打转直接读当前连续计数（无闩锁）——历史以本轮 tool_results 收口
            // （N:N 配对完整），下一轮请求不再发出，与 max_turns_reached 同款
            // 的轮边界语义。
            if self.cancelled() {
                stop_reason = "cancelled".into();
                break;
            }
            if stuck_len >= STUCK_STOP_AT {
                stop_reason = "stuck_loop".into();
                break;
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

/// Hooks gate for one pending tool call: the installed layer's decision, or
/// `Allow` when no layer is set (the default — zero overhead beyond one
/// Option check). Callers hold the ORIGINAL input for audit; only execution
/// may see a `Modify` replacement. Gates run where they're called: on the
/// parent thread for the top-level turn — serial by construction, before any
/// parallel worker spawns — and on a worker thread inside a sub-agent (see
/// the `Hooks` doc, "Concurrency"). A free function (was an `Agent` method;
/// it only ever read `&self.ctx`) so [`execute_tool_call`] and the parallel
/// batch's gate-before-spawn phase share one gate without borrowing the agent.
pub(crate) fn gate_tool_call(ctx: &ToolCtx, name: &str, input: &Value) -> ToolDecision {
    match &ctx.hooks {
        Some(h) => h.before_tool(name, input, ctx),
        None => ToolDecision::Allow,
    }
}

/// Cancel checkpoint at the gate→execution boundary (external re-review
/// P2): true when the run's cancel flag is installed and set. `before_tool`
/// can deliberate for an unbounded time (an approval UI waiting on a human),
/// so a cancel that lands in that window must be re-checked BEFORE the tool
/// runs — otherwise the run's final state says "cancelled" while the
/// approved tool still executed. Used by [`execute_tool_call`], the parallel
/// batch's spawn gate, and `run_code`'s inner dispatch so every dispatch
/// path shares one checkpoint. Relaxed load: a few ms of visibility lag is
/// irrelevant next to the ~10ms interrupt granularity elsewhere.
pub(crate) fn ctx_cancelled(ctx: &ToolCtx) -> bool {
    ctx.cancel
        .as_ref()
        .is_some_and(|c| c.load(Ordering::Relaxed))
}

/// Hooks after-notification for a finished tool call. `input` is always the
/// ORIGINAL (never a `Modify` replacement); `executed` is false for a denied
/// call and for one skipped by the gate→execution cancel checkpoint (both
/// still get their error output reported).
pub(crate) fn notify_tool_after(
    ctx: &ToolCtx,
    name: &str,
    input: &Value,
    output: &ToolOutput,
    executed: bool,
) {
    if let Some(h) = &ctx.hooks {
        h.after_tool(name, input, output, executed, ctx);
    }
}

/// Locate a tool by name in the cached `defs`, returning its index into the
/// parallel `tools` vec. Shared by the inline leaf pass (capability routing
/// needs the live tool to read `concurrency()`) and [`run_tool`] — one shared
/// routine instead of two divergent lookups. (A Sequential call is thus looked
/// up twice per invocation — once to route, once to dispatch; the tool count is
/// tiny, so this stays negligible.) Matching on `tool.def().name` instead would
/// deep-clone every probed tool's name/description/schema on EVERY call.
fn find_tool(defs: &[ToolDef], name: &str) -> Option<usize> {
    defs.iter().position(|d| d.name == name)
}

/// Look up a tool by name ([`find_tool`]) and run it through `run_with_emit`
/// (so tools with progress to stream — the `task` sub-agent — can emit into
/// the parent run). A missing tool or a tool Err becomes a `ToolOutput::error`
/// rather than aborting the run — the result is still paired N:N with the
/// tool_use that triggered it. Shared by [`execute_tool_call`], the parallel
/// batch workers, and `run_code`'s inner dispatch (which hand-rolls the
/// gate → intercept → run → after sequence so the deny_paths layer can sit
/// between the hooks gate and execution — design §7).
pub(crate) fn run_tool(
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
            Err(e) => {
                // An error message enters model-visible history, so it must
                // be size-bounded like any other tool output: a tool (e.g.
                // an MCP server) returning an arbitrarily long error string
                // used to bypass max_output_chars entirely.
                let mut msg = format!("{e:#}");
                crate::tools::truncate(&mut msg, ctx.max_output_chars);
                ToolOutput::error(msg)
            }
        },
        None => ToolOutput::error(format!("unknown tool: {name}")),
    }
}

/// One complete inline tool call: `ToolCall` event → hooks gate →
/// [`run_tool`] → after-notification, returning the output. Deliberately does
/// NOT emit the `ToolResult` event — that belongs to the caller: the loop's
/// assembly pass emits it for every slot in block order (it owns N:N pairing
/// and the cancelled-slot synthesis), and the planned `run_code` inner
/// dispatch will emit it right after each call returns (docs/run_code_design.md
/// §4). Records push / usage folding likewise stay with the caller. The
/// sequential leaf pass is the current user; the parallel batch cannot use
/// this wholesale (its contract is gate-before-spawn on the parent thread, run
/// on workers, after-join), so it composes [`gate_tool_call`] /
/// [`notify_tool_after`] / [`run_tool`] by hand instead.
pub(crate) fn execute_tool_call(
    tools: &[Box<dyn Tool>],
    defs: &[ToolDef],
    ctx: &ToolCtx,
    name: &str,
    input: &Value,
    emit: &mut dyn FnMut(AgentEvent),
    effective_out: &mut Option<Value>,
) -> ToolOutput {
    emit(AgentEvent::ToolCall {
        name: name.to_string(),
        input: input.clone(),
    });
    // Hooks gate (serial on the caller's thread). Deny → an error output that
    // the caller STILL records (N:N pairing + honest audit log) — the
    // reserved-for-cancel slot semantics live in the caller, not here.
    match gate_tool_call(ctx, name, input) {
        ToolDecision::Deny(mut reason) => {
            // Same output budget as any tool error (run_tool's MCP path): a
            // host's deny reason is model-visible is_error text and can be
            // arbitrarily long.
            crate::tools::truncate(&mut reason, ctx.max_output_chars);
            let output = ToolOutput::error(reason);
            notify_tool_after(ctx, name, input, &output, false);
            output
        }
        decision => {
            // Allow → original input; Modify → replacement. The audit trail
            // (records, ToolCall events) keeps the ORIGINAL in both cases.
            // Cancel checkpoint (external re-review P2): a cancel that landed
            // while the gate deliberated (an approval wait) must not still
            // execute the tool. The caller's Cancelled-slot synthesis uses the
            // same "[cancelled by user]" text, so history, events, and audit
            // all agree the call never ran; `after_tool` fires with
            // executed=false like a Deny.
            if ctx_cancelled(ctx) {
                let output = ToolOutput::error("[cancelled by user]");
                notify_tool_after(ctx, name, input, &output, false);
                return output;
            }
            let effective: &Value = match &decision {
                ToolDecision::Modify(v) => v,
                _ => input,
            };
            // Report a Modify replacement so the caller's stuck detector keys
            // on what ACTUALLY ran, not the model's original arguments.
            if let ToolDecision::Modify(v) = &decision {
                *effective_out = Some(v.clone());
            }
            let output = run_tool(tools, defs, name, effective, ctx, emit);
            notify_tool_after(ctx, name, input, &output, true);
            output
        }
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

/// 打转检测的调用键：把 tool input 递归排序对象键后序列化。serde_json 开了
/// preserve_order（对象保持插入序），裸 `to_string` 会因模型重发同参调用时
/// 的键序抖动把同一次调用误判成不同调用，护栏失效——比较前必须规范化。
fn canon_input(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut pairs: Vec<(String, String)> = map
                .iter()
                .map(|(k, val)| (k.clone(), canon_input(val)))
                .collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            let inner: Vec<String> = pairs
                .into_iter()
                .map(|(k, s)| format!("{}:{s}", serde_json::to_string(&k).unwrap_or_default()))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(arr) => {
            let inner: Vec<String> = arr.iter().map(canon_input).collect();
            format!("[{}]", inner.join(","))
        }
        // 数值统一经 f64 归一：provider/gateway 重发同参调用时数值类型会
        // 抖动（一处给 1、另一处给 1.0，字面不同、语义相同），裸
        // `to_string` 会把它们判成不同参数、打转漏检。f64 格式化对两者
        // 输出同一个 "1"；超过 2^53 的整数在此有精度损失，只影响指纹
        // 区分度（不同大数折叠为同键 → 宁漏检、不误杀）。
        Value::Number(n) => n
            .as_f64()
            .map(|f| f.to_string())
            .unwrap_or_else(|| n.to_string()),
        other => other.to_string(),
    }
}

/// 打转键的结果指纹：文本 content + is_error 的散列。同一调用给出**不同**
/// 结果（状态轮询的进度在推进）即视为有进展、连续计数重置——只有
/// 「同参 + 同果」才算原地打转（外审第四轮 P2：此前只比名字与参数，
/// 10%→60% 的健康轮询被误判 stuck_loop）。指纹不涵盖 image 内容（刻意，
/// 见护栏说明的已知盲区）。
fn stuck_result_hash(output: &ToolOutput) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    output.is_error.hash(&mut h);
    output.content.hash(&mut h);
    h.finish()
}

/// Repair tool_use ids before an assistant message enters history: a
/// gateway may omit the id (empty string — every result then "pairs"
/// ambiguously) or repeat one (a duplicate id makes the N:N
/// tool_use ↔ tool_result mapping ambiguous); strict APIs (Anthropic)
/// reject both. Called before `messages.push`, so the saved history, the
/// `tool_uses` tuples collected from the same blocks, and the
/// tool_results built from those tuples all agree. Minted ids live in a
/// `toolu_repair_…` namespace and are checked against everything seen so
/// far — INCLUDING `history` — so the repair itself can't introduce a
/// collision, across turns as well as within one response: the mint is
/// deterministic (`toolu_repair_{block_index}_{n}`), so a gateway emitting
/// a broken id two turns in a row used to re-mint the SAME fresh id both
/// times, seeding the history with a duplicate no later repair could fix
/// (every later request 400s until a fold rewrites the region).
fn normalize_tool_use_ids(blocks: &mut [Block], history: &[Message]) {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in history {
        for b in &m.content {
            if let Block::ToolUse { id, .. } = b {
                if !id.is_empty() {
                    seen.insert(id.clone());
                }
            }
        }
    }
    for (i, b) in blocks.iter_mut().enumerate() {
        let Block::ToolUse { id, .. } = b else {
            continue;
        };
        if !id.is_empty() && seen.insert(id.clone()) {
            continue; // unique and non-empty: keep the provider's id
        }
        let mut n = 0u32;
        let mut fresh = format!("toolu_repair_{i}_{n}");
        while !seen.insert(fresh.clone()) {
            n += 1;
            fresh = format!("toolu_repair_{i}_{n}");
        }
        *id = fresh;
    }
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

/// Per-request timeout for one summary chat call: the caller's even share of
/// either the remaining run budget (minus [`compress::COMPRESS_RESERVE`]) or,
/// without a run budget, of the [`STREAM_BACKSTOP`] leak guard. Both branches
/// floor at 1s — a base64-heavy history chunked into more pieces than the
/// budget has seconds would otherwise shrink each share below one second (a
/// zero Duration is an instant timeout: the summary fails before the request
/// is even sent), exactly when compression is most needed. Same 1s-floor
/// convention as `clamp_child_timeout`; the no-deadline branch originally
/// missed it (external re-review).
fn summary_timeout_share(deadline: Option<Instant>, n_calls: usize) -> Duration {
    let budget = match deadline {
        Some(d) => d
            .saturating_duration_since(Instant::now())
            .saturating_sub(compress::COMPRESS_RESERVE),
        None => STREAM_BACKSTOP,
    };
    (budget / (n_calls as u32)).max(Duration::from_secs(1))
}

/// Mint a message id that isn't already used in `messages`. The counter alone
/// would suffice for a fresh agent, but a RESUMED history carries ids minted
/// by an earlier process whose counter started over — scan for collisions and
/// bump past them (host-supplied custom ids are respected too; uniqueness
/// among THOSE is the host's contract). O(n) per mint, only on ingest of new
/// messages — negligible against the requests it brackets.
fn fresh_message_id(messages: &[Message], next: &mut u64) -> String {
    loop {
        // saturating per the numeric-writer convention; u64::MAX messages in
        // one history is not a real scenario (and re-issuing the max id then
        // is the least of the problems).
        *next = next.saturating_add(1);
        let id = format!("m{}", *next);
        if !messages
            .iter()
            .any(|m| m.id.as_deref() == Some(id.as_str()))
        {
            return id;
        }
    }
}

/// The numeric suffix of a minted `mN` id; anything else (host-supplied id,
/// None) is None. Used to keep the mint counter ahead of the archive. Public
/// so the CLI bin can pre-mint a resumed task's id above history + archive
/// (see main.rs).
pub fn numeric_id_suffix(id: &str) -> Option<u64> {
    id.strip_prefix('m')?.parse().ok()
}

/// Assign ids to every id-less message (ingest point for constructed /
/// resumed histories). Messages that already carry one — from a previous
/// save, or supplied by the host — keep it untouched.
fn assign_missing_ids(messages: &mut [Message], next: &mut u64) {
    for i in 0..messages.len() {
        if messages[i].id.is_none() {
            let id = fresh_message_id(messages, next);
            messages[i].id = Some(id);
        }
    }
}
impl Agent {
    /// Host-initiated history compression — the idle-time entry point the
    /// preflight/overflow paths don't offer. A GUI whose context gauge says
    /// 80% wants to fold NOW, not after the next request trips a threshold or
    /// (worse) a 400. Semantics are the manual twin of the run-loop
    /// compression: rolling summary + `find_safe_cut` + `fold_summary`, the
    /// session file is rewritten, and a `Compressed { trigger: Manual }`
    /// event fires through `events`. The size threshold does NOT apply — the
    /// caller IS the decision.
    ///
    /// `instructions` is the host's focus hint (Claude Code's
    /// `/compact focus on X`): free-form text appended to the summary prompt
    /// telling the summarizer what to preserve with extra care ("focus on
    /// the API changes"). `None`/empty folds with the stock prompt. It rides
    /// the user prompt, not SUMMARY_SYSTEM — the system prompt is shared by
    /// every summary call of every compaction, while the hint belongs to
    /// this one fold.
    ///
    /// Call this BETWEEN runs: the `&mut self` signature makes a concurrent
    /// `run` unrepresentable, so the compiler enforces the sequencing. A
    /// stale `cancel` flag from a previously-cancelled run is cleared at
    /// entry (same rationale as `run`'s reset — otherwise the summary request
    /// would abort instantly on a flag nobody wanted anymore), and a stale
    /// `compress_failed` is cleared too: a manual call is a fresh, visible
    /// attempt, and silently no-op'ing the host's button would look broken.
    ///
    /// Returns:
    /// - `Ok(Some(outcome))` — folded; `dropped == 0` is the degenerate
    ///   empty-summary pass (billed, nothing folded, a Warning said why);
    /// - `Ok(None)` — nothing compressible (no safe cut: history too short
    ///   for `keep_recent_messages`);
    /// - `Err` — the summary request failed; compression is disabled for the
    ///   next run until it resets at `run` entry (or another `compact`).
    ///
    /// Cost accounting: the summary's `usage` is returned in the outcome and
    /// deliberately NOT folded into [`Agent::usage`] — those totals reset at
    /// every `run` entry, so folding it in would vanish at the next run. The
    /// HOST owns this cost (persist it, display it, meter it); it will not
    /// appear in the next `RunOutcome` or result.json.
    pub fn compact_with(
        &mut self,
        events: &mut dyn FnMut(AgentEvent),
        instructions: Option<&str>,
    ) -> Result<Option<CompressOutcome>> {
        // Fresh-attempt resets (see the doc). Order: clear the disable flag,
        // then cancel — a summary chat consults the flag live. Clearing here
        // is the acceptance-point pattern (see clear_cancel): the host
        // explicitly starting a manual compact is accepting new work.
        self.compress_failed = false;
        self.clear_cancel();
        // No run deadline: summary_call falls back to its per-request
        // backstop (STREAM_BACKSTOP), so a stuck summary can't hang forever.
        // The CompressError (if any) converts to anyhow here — Display
        // delegates to its source, so the host-visible message is unchanged;
        // `burned` stays downcastable on the chain for a binding to surface.
        self.compress(
            events,
            /* deadline: */ None,
            CompressTrigger::Manual,
            instructions,
        )
        .map_err(anyhow::Error::new)
    }

    /// [`Agent::compact_with`] without a focus hint — the common fold.
    pub fn compact(
        &mut self,
        events: &mut dyn FnMut(AgentEvent),
    ) -> Result<Option<CompressOutcome>> {
        self.compact_with(events, None)
    }

    /// Preflight "is the history over the compaction threshold?" — a token
    /// estimate against `max_context_tokens` when configured, else the legacy
    /// `compress_threshold_chars` heuristic. `None` when neither knob is set:
    /// the caller can't estimate and must treat the provider's verdict (the
    /// overflow error) as the only signal.
    fn over_compaction_threshold(&self) -> Option<bool> {
        if self.limits.max_context_tokens > 0 {
            let est = self.estimate_input_tokens();
            // Trip at COMPACT_UTILIZATION_PCT of the window, reserving room
            // for this turn's max_tokens output — so we compact BEFORE the
            // provider would reject an over-long request, not after. Compute
            // on u64 with saturating_mul so an absurd configured value can't
            // wrap (real windows are millions, far from overflow). The usize
            // conversion goes through try_from, never `as` — on the 32-bit
            // musl build a plain cast would truncate a big configured window
            // into a tiny `cap` and flip the comparison.
            let cap = usize::try_from(
                self.limits
                    .max_context_tokens
                    .saturating_mul(compress::COMPACT_UTILIZATION_PCT)
                    / 100,
            )
            .unwrap_or(usize::MAX);
            let out_room = usize::try_from(self.max_tokens).unwrap_or(usize::MAX);
            Some(est.saturating_add(out_room) > cap)
        } else if self.limits.compress_threshold_chars > 0 {
            Some(compress::total_chars(&self.messages) > self.limits.compress_threshold_chars)
        } else {
            None
        }
    }

    /// Estimated prompt tokens for the next request. Prefers the measured
    /// base: the provider-reported prompt size of the last main-chat request
    /// plus a chars-per-token estimate of only the messages appended since.
    /// Falls back to the whole-history heuristic when no anchor exists (first
    /// turn, resumed session, history rewritten by prune/compaction, provider
    /// reports no usage) or when the prefix changed since the measurement —
    /// both strictly conservative directions.
    fn estimate_input_tokens(&self) -> usize {
        if let Some(a) = &self.usage_anchor {
            let valid = self.messages.len() >= a.msg_count
                && compress::total_chars(&self.messages[..a.msg_count]) == a.prefix_chars
                && self.system.as_ref().map(|s| s.len()) == a.system_chars
                && self.tool_schema_chars == a.tool_schema_chars;
            if valid {
                let delta = compress::estimate_tokens(
                    &self.messages[a.msg_count..],
                    /* system: */ None,
                    /* tool_schema_chars: */ 0,
                );
                return usize::try_from(a.prompt_tokens)
                    .unwrap_or(usize::MAX)
                    .saturating_add(delta);
            }
        }
        compress::estimate_tokens(
            &self.messages,
            self.system.as_deref(),
            self.tool_schema_chars,
        )
    }

    /// One prune pass over the history with the configured protection window
    /// (see [`compress::prune_tool_results`]): stale oversized tool results
    /// outside the `keep_recent_messages` window become placeholders. Returns
    /// the chars freed (`0` = nothing pruned). No LLM call, no billing.
    fn try_prune(&mut self, events: &mut dyn FnMut(AgentEvent)) -> usize {
        self.try_prune_protect(events, self.limits.keep_recent_messages)
    }

    /// [`try_prune`] with an explicit protection window — the overflow
    /// backstop shrinks it below the configured value when the kept tail
    /// alone overflows a small window.
    fn try_prune_protect(
        &mut self,
        events: &mut dyn FnMut(AgentEvent),
        protect_recent: usize,
    ) -> usize {
        let Some((pruned, touched, freed)) =
            compress::prune_tool_results(&self.messages, protect_recent)
        else {
            return 0;
        };
        let originals: Vec<Message> = touched.iter().map(|&i| self.messages[i].clone()).collect();
        self.archive_messages(originals);
        self.messages = pruned;
        // History rewritten ⇒ the usage anchor (a real provider measurement
        // of a history that no longer exists) is void.
        self.usage_anchor = None;
        if let Some(w) = self.save_session() {
            events(AgentEvent::Warning(w));
        }
        events(AgentEvent::Pruned {
            messages: touched.len(),
            chars_freed: freed,
        });
        freed
    }

    /// Append messages leaving the live history (compression fold / prune) to
    /// the session archive, FIFO-capped. Ids ride along, so a host can anchor
    /// undo/audit assets to the same stable ids the live history uses.
    ///
    /// Dedup by id, keeping the FIRST copy: a prune archives a message's
    /// ORIGINAL and leaves a placeholder live, and a later fold of that same
    /// message must not file the placeholder version next to the original —
    /// the original is the one worth keeping.
    ///
    /// The id counter is pushed past every archived id: fresh mints fill gaps
    /// by skipping collisions against the LIVE history only, so without this
    /// a resumed session would re-issue the ids of long-folded messages and
    /// collide with their archived originals.
    fn archive_messages<I: IntoIterator<Item = Message>>(&mut self, msgs: I) {
        for m in msgs {
            let dup = m.id.as_deref().is_some_and(|id| {
                self.session_archive
                    .iter()
                    .any(|a| a.id.as_deref() == Some(id))
            });
            if !dup {
                self.session_archive.push(m);
            }
        }
        let overflow = self
            .session_archive
            .len()
            .saturating_sub(ARCHIVE_MAX_MESSAGES);
        if overflow > 0 {
            self.session_archive.drain(0..overflow);
        }
        self.bump_id_counter_past_archive();
    }

    /// Advance `next_msg_id` past every numeric id in the archive (see
    /// [`archive_messages`]). Ids are host-suppliable strings; non-`mN`
    /// shapes simply don't parse and are ignored.
    fn bump_id_counter_past_archive(&mut self) {
        let floor = self
            .session_archive
            .iter()
            .filter_map(|m| m.id.as_deref().and_then(numeric_id_suffix))
            .max()
            .unwrap_or(0);
        if floor > self.next_msg_id {
            self.next_msg_id = floor;
        }
    }

    /// Compress history if it's too large, summarizing the middle via the LLM
    /// and keeping only the head (+ folded summary) and recent tail. Failures
    /// degrade gracefully for the run loop (warn and continue uncompressed).
    ///
    /// `trigger` picks both the gate and the report: `Preflight` gates on the
    /// size threshold (token estimate against `max_context_tokens` when set,
    /// else the legacy char heuristic); `Overflow` and `Manual` skip it — the
    /// provider has just PROVEN the request too long, or the host has decided
    /// it wants the fold now regardless of size.
    ///
    /// Returns:
    /// - `Ok(None)` — no compression ran (below threshold / out of budget /
    ///   nothing compressible / disabled by an earlier failure);
    /// - `Ok(Some(outcome))` — ran; `outcome.usage` is `None` when the
    ///   provider reported no usage (count nothing, emit no Usage event).
    ///   `dropped == 0` is the degenerate empty-summary pass (billed, nothing
    ///   folded, compression now disabled for the run);
    /// - `Err(CompressError)` — the summary request itself failed;
    ///   `burned` carries whatever it billed first (see `CompressError`).
    ///   `compress_failed` is set (inside), but NO Warning is emitted here:
    ///   the two callers disagree on the failure channel — the run loop
    ///   reports it as an event (and bills `burned`), the host-facing
    ///   `compact` propagates the `Err` — so emitting it here would
    ///   double-report on one of them.
    fn compress(
        &mut self,
        events: &mut dyn FnMut(AgentEvent),
        deadline: Option<Instant>,
        trigger: CompressTrigger,
        instructions: Option<&str>,
    ) -> std::result::Result<Option<CompressOutcome>, CompressError> {
        // Threshold gate. Overflow/Manual skip it — the provider just proved
        // the request too long, or the host asked for the fold explicitly.
        // Otherwise the trigger depends on config: a token estimate against
        // max_context_tokens when set, else the legacy char heuristic.
        let force = !matches!(trigger, CompressTrigger::Preflight);
        if !force {
            if self.compress_failed || self.over_compaction_threshold() != Some(true) {
                return Ok(None);
            }
            // Cheap gate first: placeholder-prune stale oversized tool
            // results and re-check — a history whose bulk is old tool output
            // often fits again without a billed summary call.
            if self.try_prune(events) > 0 && self.over_compaction_threshold() != Some(true) {
                return Ok(None);
            }
        } else if self.compress_failed {
            return Ok(None);
        }
        // Compression is best-effort: never let the summary request eat the
        // run's remaining budget — that would leave the main request with a
        // zero/near-zero timeout and fail the whole run with no result file.
        // Skip unless there's room for both the summary and the main request.
        if let Some(d) = deadline {
            if d.saturating_duration_since(Instant::now())
                < compress::COMPRESS_RESERVE + Duration::from_secs(5)
            {
                return Ok(None);
            }
        }
        let cut = match compress::find_safe_cut(&self.messages, self.limits.keep_recent_messages) {
            Some(c) => c,
            None => return Ok(None),
        };

        let (summary, sum_usage) = match self.summarize_prefix(cut, deadline, instructions) {
            Ok((s, u)) if !s.trim().is_empty() => (s, u),
            Ok((_empty, u)) => {
                // The summary request ran (so compression "happened") even
                // though the model returned empty/unusable text. Pass `u`
                // through so the caller can bill it honestly; `dropped: 0`
                // tells the caller nothing was folded. Either way disable
                // compression for the rest of this run (don't retry every
                // turn), and WARN — a silent disable here (unlike the Err arm
                // below) would leave a growing context with no explanation
                // for why it stops compacting. Fires once: compress_failed
                // gates future attempts.
                self.compress_failed = true;
                events(AgentEvent::Warning(
                    "context compression produced an empty summary (disabled for this run)".into(),
                ));
                return Ok(Some(CompressOutcome {
                    dropped: 0,
                    kept: self.messages.len(),
                    usage: u,
                }));
            }
            Err(e) => {
                self.compress_failed = true;
                // No Warning here — see the fn doc (caller-owned channel).
                return Err(e);
            }
        };

        let dropped = cut - 1;
        // The dropped middle's ORIGINALS move to the session archive (the
        // head stays live in folded form, the tail verbatim).
        let dropped_middle: Vec<Message> = self.messages[1..cut].to_vec();
        self.archive_messages(dropped_middle);
        self.messages = compress::fold_summary(&self.messages, cut, &summary);
        // The folded history shares no measurement with the pre-fold one —
        // the anchor is void (same as after a prune).
        self.usage_anchor = None;
        // The fold constructs fresh messages (the summary carrier, and a fresh
        // user head when the history opens on an assistant) with no id; kept
        // head/tail messages carry theirs through the clone. Mint for the
        // new entries so the post-compress history stays fully anchored.
        assign_missing_ids(&mut self.messages, &mut self.next_msg_id);
        if let Some(w) = self.save_session() {
            events(AgentEvent::Warning(w));
        }
        events(AgentEvent::Compressed {
            dropped,
            kept: self.messages.len(),
            trigger,
        });
        // The summary is a real billed request — `usage` travels in the
        // outcome so the caller decides the ledger: the run loop folds it
        // into its totals (bill_chat), the host-facing compact returns it
        // for the host's own metering (self.usage resets per run, so folding
        // it here would silently vanish at the next run entry).
        Ok(Some(CompressOutcome {
            dropped,
            kept: self.messages.len(),
            usage: sum_usage,
        }))
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
    /// call (single, or per-chunk + combine). `instructions` is the manual
    /// compact's host focus hint — applied only to the FINAL summary call (in
    /// the chunked path the per-chunk calls stay stock so the hint weights the
    /// product, not every intermediate). On failure the error carries ALL
    /// tokens burned so far (earlier chunks + the dying call — see
    /// [`CompressError`]), so a chunk 3 failure doesn't drop chunks 1-2's
    /// billing on the floor.
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
        instructions: Option<&str>,
    ) -> std::result::Result<(String, Option<Usage>), CompressError> {
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
                &compress::with_focus_instructions(
                    &compress::summary_prompt(&transcript, old_summary.as_deref()),
                    instructions,
                ),
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
                    &compress::with_focus_instructions(
                        &compress::summary_prompt(&chunks[0], old_summary.as_deref()),
                        instructions,
                    ),
                    compress::summary_budget(chunks[0].len()).min(cap),
                    deadline,
                    1,
                );
            }
            let n_calls = chunks.len() + 1; // per-chunk + combine
            let mut usage = None;
            let mut chunk_summaries = Vec::with_capacity(chunks.len());
            for chunk in &chunks {
                let (s, u) = match self.summary_call(
                    &compress::summary_prompt(chunk, None),
                    compress::SUMMARY_CHUNK_BUDGET.min(cap),
                    deadline,
                    n_calls,
                ) {
                    Ok(v) => v,
                    // Fold this chunk's burn ON TOP of the earlier chunks'
                    // already-accounted usage (`usage` local) — all of it is
                    // real billed cost of the failed compression.
                    Err(e) => {
                        return Err(CompressError {
                            burned: compress::merge_usage(usage, e.burned),
                            source: e.source,
                        })
                    }
                };
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
            let (final_summary, u) = match self.summary_call(
                &compress::with_focus_instructions(
                    &compress::summary_prompt(&combined, old_summary.as_deref()),
                    instructions,
                ),
                budget,
                deadline,
                n_calls,
            ) {
                Ok(v) => v,
                Err(e) => {
                    return Err(CompressError {
                        burned: compress::merge_usage(usage, e.burned),
                        source: e.source,
                    })
                }
            };
            usage = compress::merge_usage(usage, u);
            Ok((final_summary, usage))
        }
    }

    /// One summary chat call (no tools, no cache). `n_calls` shares the
    /// remaining run budget evenly across all summary calls of this compression
    /// so a multi-chunk summarize can't eat the main turn's time budget.
    /// Failures leave as [`CompressError`] carrying whatever was billed first.
    fn summary_call(
        &self,
        prompt: &str,
        max_tokens: u32,
        deadline: Option<Instant>,
        n_calls: usize,
    ) -> std::result::Result<(String, Option<Usage>), CompressError> {
        let mut max_tokens = max_tokens;
        // Honest cost: a burned first attempt still bills, so usage merges
        // across attempts and the caller's ledger sees every request sent.
        let mut spent: Option<Usage> = None;
        for attempt in 0..2 {
            let req = ChatRequest {
                system: Some(compress::SUMMARY_SYSTEM),
                messages: &[Message::user(vec![Block::Text(prompt.to_string())])],
                tools: &[],
                model: &self.model,
                max_tokens,
                // Even share of the remaining run budget (minus the reserve)
                // — or of the STREAM_BACKSTOP leak guard without one —
                // across all summary calls, floored at 1s so a huge chunk
                // count can't shrink a share into an instant timeout (see
                // summary_timeout_share).
                per_request_timeout: Some(summary_timeout_share(deadline, n_calls)),
                cancel: self.cancel_flag(),
                // The summary is one-off: the next compression summarizes a
                // different transcript, so a cache breakpoint would only ever pay
                // the 1.25× write premium with no subsequent read. Skip caching.
                cache: false,
                // Summaries don't reason — drop the thinking config so a reasoning
                // model doesn't burn a thinking trace on a compression call.
                // (NOT a low-effort value: on Anthropic any Some(non-"none")
                // effort ENABLES adaptive thinking — the opposite of the intent —
                // and on unknown OpenAI-compatible gateways "low" is a coin flip.
                // Gateways that default to thinking anyway are handled by the
                // empty-body retry below, provider-agnostically.)
                reasoning_effort: None,
            };
            // One transient retry (Codex-style backoff): a single lost
            // packet/5xx on the summary request currently disables
            // compression for the WHOLE run (compress_failed) — an expensive
            // loss for one blip. Deterministic failures don't retry: a
            // summary that itself overflows the window won't fit after 1s
            // either, and a cancel flag already set means the user asked to
            // stop. Cancel mid-request doesn't arrive as an Err at all
            // (it maps to a stop_reason, handled below), so no cancel-shape
            // sniffing here.
            let mut request_attempt = 0u32;
            let resp = loop {
                match self.provider.chat(&req, &mut |_| {}) {
                    Ok(r) => break r,
                    Err(e) => {
                        if request_attempt == 0
                            && !compress::is_context_overflow(&e)
                            && !self.cancelled()
                        {
                            request_attempt += 1;
                            let flag = self.cancel_flag();
                            crate::retry::sleep_cancelable(
                                &flag,
                                crate::retry::backoff(/* attempt: */ 0),
                            );
                            continue;
                        }
                        // Honest cost: a provider Err can still have burned
                        // tokens (usage reported mid-stream, before the
                        // error) — pull the RunError's usage out of the
                        // chain and return it alongside `spent` instead of
                        // dropping both on the floor (same discipline as the
                        // failed-attempt folding in run_with_retries).
                        let burned = compress::merge_usage(
                            spent,
                            e.downcast_ref::<RunError>().map(|r| r.usage),
                        );
                        return Err(CompressError { source: e, burned });
                    }
                }
            };
            spent = compress::merge_usage(spent, resp.usage);
            // parse_loop reports cancel/idle-timeout/truncation as Ok with a
            // non-end_turn stop_reason and PARTIAL, half-written blocks. Folding
            // such a summary into the history would poison every later turn, so
            // bail on those. max_tokens is DIFFERENT: the model stopped at the
            // output budget, so the text is coherent (just incomplete) — accepting
            // it beats disabling compression, because compress_failed also gates
            // the overflow-recovery path (two failures for the price of one).
            // EXCEPT the zero-body shape: hybrid-thinking gateways (deepseek /
            // glm) bill reasoning_content against the same max_tokens, and an
            // UPDATE-branch prompt can induce a long enough trace to consume the
            // ENTIRE budget — finish=length with zero content characters (usage
            // shows output == budget exactly; TinyWorker acceptance #7). "Truncated
            // but coherent" is false for an empty body, so retry ONCE with a
            // doubled budget, which leaves room for the same trace plus a body.
            // A genuine long-text truncation (non-empty body) is still accepted on
            // the first attempt; a retry that truncates again is accepted too.
            match resp.stop_reason.as_str() {
                "end_turn" | "max_tokens" => {}
                other => {
                    // `spent` was merged above — return it rather than bail it
                    // away: a cancelled/interrupted summary still billed its
                    // partial stream.
                    return Err(CompressError {
                        source: anyhow::anyhow!(
                            "summary request did not complete (stop_reason={other})"
                        ),
                        burned: spent,
                    });
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
            if attempt == 0 && resp.stop_reason == "max_tokens" && text.trim().is_empty() {
                // Deliberately NOT clamped to the caller's turn cap: the first
                // attempt just proved a cap-sized budget dies to reasoning, so
                // clamping would repeat a known-doomed request. A strict gateway
                // that 400s on the doubled value fails this call (≈ today's
                // degradation, one cheap request later). 8192 ceiling keeps the
                // retry under common model output caps.
                max_tokens = max_tokens.saturating_mul(2).min(8192);
                continue;
            }
            return Ok((text, spent));
        }
        unreachable!("attempt 1 always returns");
    }
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;
