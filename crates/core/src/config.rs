use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};

fn default_max_turns() -> u32 {
    30
}
fn default_timeout() -> u64 {
    600
}
fn default_max_output() -> usize {
    30_000
}
fn default_compress_threshold() -> usize {
    // ≈100k tokens at 4 chars/token: sized for modern 128k–1M windows. Small
    // (32k-class) windows overshoot it, but the always-on overflow recovery
    // (force-compress + retry on the provider's 400) is the backstop there.
    400_000
}
fn default_keep_recent() -> usize {
    8
}
fn default_stream_idle() -> u64 {
    30
}
fn default_shell_timeout() -> u64 {
    120
}
fn default_subagent_max_turns() -> u32 {
    25
}
fn default_max_tokens() -> u32 {
    8192
}
fn default_inject_environment() -> bool {
    true
}
fn default_salvage_on_max_tokens() -> bool {
    true
}
fn default_tools() -> Vec<String> {
    vec![
        "shell".into(),
        "read_file".into(),
        "write_file".into(),
        "edit_file".into(),
        "list_dir".into(),
        // `task` spawns a focused sub-agent (independent history + turn budget)
        // for decomposed/parallel work. Enabled by default so a TUI conversation
        // or headless run can delegate sub-tasks without extra config; it is
        // non-recursive (a sub-agent's tool whitelist drops `task` itself).
        "task".into(),
        // `todo` lets the agent track its plan in a structured, audit-friendly
        // way: the final list lands in result.json's `plan` field. Enabled by
        // default (matches `task`); eval/CI wanting to lock behavior removes it
        // from `tools`. Not inherited by sub-agents (filtered in subagent.rs).
        "todo".into(),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_output")]
    pub max_output_chars: usize,
    /// Compress conversation history beyond this many chars (0 = never).
    /// Only the FALLBACK trigger: used when `max_context_tokens` is 0/unset.
    /// Once `max_context_tokens > 0`, the token-based preflight estimate takes
    /// over and this char threshold is ignored.
    #[serde(default = "default_compress_threshold")]
    pub compress_threshold_chars: usize,
    /// Messages kept verbatim at the tail when compressing.
    #[serde(default = "default_keep_recent")]
    pub keep_recent_messages: usize,
    /// Model context-window ceiling in tokens. `> 0` switches preflight
    /// compression to a token estimate (≈4 chars/token, reserving room for one
    /// turn's output) that compacts before the provider would reject an
    /// over-long request; `0` (default) keeps the legacy `compress_threshold_chars`
    /// heuristic. Overflow compression is ALWAYS on regardless of this field —
    /// it is the last-resort safety net and never depends on client config.
    #[serde(default)]
    pub max_context_tokens: u64,
    /// Whole-run token budget: input + output + cache creation + cache read,
    /// summed across every turn, sub-agent, and compression call. When the
    /// accumulated total crosses this, the run stops with `stop_reason:
    /// "budget_exceeded"` (exit 125) instead of starting another turn. `0`
    /// (default) = no cap. Checked at turn boundaries, like `timeout_secs` —
    /// the turn that pushes the total past the cap finishes normally (its
    /// tools run, its assistant message is kept); the cap only refuses to
    /// start the NEXT turn, so the transcript and tool_use/tool_result pairing
    /// stay intact. A batch task file may only TIGHTEN it (smaller value); 0
    /// means "no cap" (the loosest) and is ignored when set by a task.
    #[serde(default)]
    pub max_total_tokens: u64,
    /// Max gap (seconds) between SSE chunks before a stream is treated as
    /// stalled and aborted. Ordinary chat models have sub-second inter-chunk
    /// gaps, so 30s is ample; deep-reasoning models with long first-token
    /// latency (DeepSeek-R1, o-series) can sit silent for minutes before the
    /// first token and need this raised. The reqwest total timeout
    /// (`timeout_secs`) remains the hard backstop regardless.
    #[serde(default = "default_stream_idle")]
    pub stream_idle_secs: u64,
    /// Max wall-clock time for a single `shell` tool command before it is
    /// killed and reported as an error. `0` (default is 120) = no per-command
    /// limit — the run-level `timeout_secs` and interactive cancel still apply.
    /// Unlike the other limits fields this is a COST/latency knob, not a safety
    /// boundary: eval tasks routinely need builds/installs longer than 120s,
    /// and a task that wants to burn time can do it with ten thousand fast
    /// commands anyway. So a batch task fragment may override it in BOTH
    /// directions (see `Clamp::Replace` — the only limits field with no clamp).
    #[serde(default = "default_shell_timeout")]
    pub shell_timeout_secs: u64,
    /// Turn ceiling for `task` sub-agents, applied as
    /// `min(parent max_turns, this)`: a sub-agent is a bounded side-quest, so
    /// without a ceiling a stuck child keeps burning the parent's loop for as
    /// long as the parent's own budget allows. `0` = no separate ceiling (the
    /// child inherits the parent's `max_turns` verbatim) — the LOOSEST value,
    /// so a batch task fragment may only tighten it (`Clamp::TightenOrInit`).
    /// Child budgets never exceed the parent's: the cap only ever lowers.
    #[serde(default = "default_subagent_max_turns")]
    pub subagent_max_turns: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_turns: default_max_turns(),
            timeout_secs: default_timeout(),
            max_output_chars: default_max_output(),
            compress_threshold_chars: default_compress_threshold(),
            keep_recent_messages: default_keep_recent(),
            max_context_tokens: 0,
            max_total_tokens: 0,
            stream_idle_secs: default_stream_idle(),
            shell_timeout_secs: default_shell_timeout(),
            subagent_max_turns: default_subagent_max_turns(),
        }
    }
}

/// How a batch task fragment may override one `Limits` field relative to the
/// trusted base config. Batch task files are untrusted: the only legal
/// direction is TIGHTENING (never loosen a resource limit — otherwise a task
/// could set timeout_secs=0 (no limit) and max_turns=u32::MAX and pin a
/// worker forever), so every limits field maps to exactly one of three
/// shapes. [`Self::merge`] is the single authoritative implementation — the
/// per-field hand-written branches `Config::apply` used to carry (including
/// four fossilized copies of the 0-sentinel if-chain) now live here.
enum Clamp {
    /// Smaller is tighter and 0 is either the tightest value or nonsensical
    /// (clamped at the use site, e.g. stream idle ≥1s in the provider): plain
    /// `min`. A fragment can only lower the base value.
    /// (max_turns / max_output_chars / keep_recent_messages / stream_idle_secs)
    Tighten,
    /// 0 means "no limit" — the LOOSEST value, opposite of [`Self::Tighten`]
    /// fields. Canonical explanation, written ONCE here (it previously
    /// fossilized as four hand-copied if-chains with long comments): a naive
    /// `min` both lets a task's 0 loosen/disable the base cap, and — when the
    /// base is 0 — collapses any finite tightening back to 0 (`v.min(0)==0`),
    /// silently dropping e.g. a 30s tightening and leaving the worker
    /// unbounded. So: a task's 0 is ignored (no-op sentinel); a 0 (unlimited)
    /// base adopts any finite task value (any finite value tightens);
    /// otherwise `min`.
    /// (timeout_secs / compress_threshold_chars / max_context_tokens /
    /// max_total_tokens)
    TightenOrInit,
    /// Cost/latency knob, not a safety boundary: overridden in BOTH
    /// directions, no clamp. A task that wants to burn time can do it with
    /// ten thousand fast commands anyway; the run-level `timeout_secs` and
    /// `max_total_tokens` remain the real backstops. (shell_timeout_secs only)
    Replace,
}

impl Clamp {
    /// Apply a task fragment (`patch`) over the trusted `base` under this
    /// policy. `None` keeps the base untouched. Generic over any ordered
    /// integer type whose `Default` is the 0 sentinel, so call sites keep
    /// their own field types with no widening/narrowing casts.
    fn merge<T: Ord + Default>(self, base: T, patch: Option<T>) -> T {
        let Some(v) = patch else { return base };
        match self {
            Clamp::Tighten => v.min(base),
            Clamp::TightenOrInit => {
                if v == T::default() {
                    base
                } else if base == T::default() {
                    v
                } else {
                    v.min(base)
                }
            }
            Clamp::Replace => v,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Anthropic,
    Openai,
}

impl ProviderKind {
    pub fn default_base_url(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "https://api.anthropic.com",
            ProviderKind::Openai => "https://api.openai.com",
        }
    }

    /// Stable lowercase label matching the serde rename ("anthropic" /
    /// "openai") — used for attribution fields in result.json / report.json.
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Openai => "openai",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    /// Try Streamable HTTP first, fall back to legacy HTTP+SSE.
    Auto,
    /// Streamable HTTP (2025-03-26).
    Http,
    /// Legacy HTTP+SSE (2024-11-05, deprecated).
    Sse,
}

// No derived Debug: `headers`/`env` are credential carriers (see the `env`
// doc below — Authorization: Bearer …, GITHUB_PERSONAL_ACCESS_TOKEN, DB
// URLs), and a derived impl would hand the VALUES to any `{:?}` — host
// logs, panic messages, error chains. The hand-written impl below keeps the
// key NAMES (diagnostic value: which header/env var a failing server was
// reached with) and masks the values (the secrets). This matches the
// indirect design the API key itself uses (`api_key_env` stores the variable
// NAME, never the value); these two maps — plus `url` userinfo
// (`https://user:pass@host` basic-auth endpoints, folded in by the follow-up
// P2 round via `redact_url_credentials`) — were the only places a raw
// secret could ride a Debug print out of this module.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub name: String,
    /// stdio transport: command to spawn.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// HTTP transports: server URL.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub transport: Option<McpTransport>,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    /// stdio transport only: extra env vars passed to the spawned child, added
    /// on top of the inherited host environment (same-name clash: config wins).
    /// Ignored for url transports. GUI apps launched without a shell rely on
    /// this to pass credentials (GITHUB_PERSONAL_ACCESS_TOKEN, DB URLs, …) to
    /// stdio MCP servers — matching the Claude Desktop / VS Code config shape.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

/// Manual Debug (see the struct comment): every field prints normally except
/// `headers`/`env`, whose VALUES are replaced with `"***"` — key names stay —
/// and `url`, whose userinfo credentials collapse to `***@` with the
/// host/port/path kept ([`redact_url_credentials`]). Threat model: a Debug
/// print is for humans/logs, and a secret that has ever appeared in one must
/// be considered burned; a key name or a host name alone reveals nothing an
/// attacker could use. `Config`/`PartialConfig` embed this type and their
/// derived Debugs route through here, so the redaction covers the whole
/// containment chain with one impl.
impl fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn redact(
            m: &std::collections::HashMap<String, String>,
        ) -> std::collections::HashMap<&str, &str> {
            m.keys().map(|k| (k.as_str(), "***")).collect()
        }
        f.debug_struct("McpServerConfig")
            .field("name", &self.name)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("url", &self.url.as_deref().map(redact_url_credentials))
            .field("transport", &self.transport)
            .field("headers", &redact(&self.headers))
            .field("env", &redact(&self.env))
            .finish()
    }
}

/// Debug-time userinfo mask for `McpServerConfig.url` (P2, extends the
/// headers/env redaction round): URLs may embed basic-auth credentials
/// (`https://user:pass@host/...`) and the url field used to print verbatim,
/// so a Debug print leaked them even with the maps already masked. Detection
/// is deliberately bounded to where userinfo can legally live: after the
/// FIRST `scheme://` (a later one belongs to a path/query value, e.g. a
/// redirect parameter), inside the AUTHORITY ONLY — the run up to the first
/// `/`, `?` or `#` (RFC 3986); an `@` past that bound is path/query text,
/// not a credential. Within the authority the LAST `@` terminates userinfo
/// (RFC; splitting at an earlier one would re-leak password fragments after
/// the mask in `user:p@ss@host`), and a NON-EMPTY userinfo — a bare
/// username, or `user:pass` with its `:` separator — is credentials: the
/// scheme prefix stays, the userinfo collapses to `***@`, and the
/// host/port/path print on. An empty `@host` userinfo carries nothing and
/// stays verbatim, as does anything the scan cannot parse.
///
/// Why hand-rolled instead of the `url` crate: it does sit in the reqwest
/// tree, but core has never depended on it directly, and this is a
/// DISPLAY-ONLY heuristic over untrusted config strings — `Url::parse` (a)
/// hard-fails on malformed input while our contract here is "display, never
/// crash", so the fallback branch would be needed anyway, and (b)
/// normalizes (percent-encoding, case, empty-component elision), which
/// would change what `{:?}` shows for BENIGN urls and regress
/// debuggability. A direct dependency plus that drift for ~15 lines of
/// bounded scanning is the worse trade; every slice below lands on a
/// separator the scan itself found, so the function cannot panic.
fn redact_url_credentials(url: &str) -> Cow<'_, str> {
    // No `scheme://` → no authority to scan (relative refs, `localhost:8080`,
    // garbage): display verbatim — the untrusted-input fallback IS display.
    let Some(scheme_end) = url.find("://") else {
        return Cow::Borrowed(url);
    };
    let after_scheme = &url[scheme_end + 3..];
    // Authority ends at the first path/query/fragment marker.
    let auth_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..auth_end];
    // LAST '@' inside the authority — the RFC userinfo terminator and the
    // fail-closed choice (see the doc comment above).
    let Some(at) = authority.rfind('@') else {
        return Cow::Borrowed(url);
    };
    // Credentials = non-empty userinfo; `@host` (empty) has nothing to leak.
    if authority[..at].is_empty() {
        return Cow::Borrowed(url);
    }
    Cow::Owned(format!(
        "{}***@{}",
        &url[..scheme_end + 3],
        &after_scheme[at + 1..]
    ))
}

impl McpServerConfig {
    pub fn validate(&self) -> Result<()> {
        match (&self.command, &self.url) {
            (Some(_), Some(_)) => {
                bail!(
                    "mcp server '{}': set either \"command\" or \"url\", not both",
                    self.name
                )
            }
            (None, None) => {
                bail!(
                    "mcp server '{}': one of \"command\" or \"url\" is required",
                    self.name
                )
            }
            _ => {}
        }
        if self.command.is_some() && (self.transport.is_some() || !self.headers.is_empty()) {
            bail!(
                "mcp server '{}': \"transport\"/\"headers\" only apply to url servers",
                self.name
            );
        }
        Ok(())
    }
}

/// Skill disclosure policy for the built-in `run-code` skill
/// (finetune_support_design FD-2). The declaration order IS the disclosure
/// ordering — `Off < OnDemand < Eager` — so batch fragments merge with
/// `Clamp::Tighten` (min): an untrusted fragment can only disclose LESS.
/// `Default = OnDemand` keeps every unconfigured user byte-identical.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillInjection {
    /// Built-in skill is not injected into the index; read_skill("run-code")
    /// returns an explicit disabled error (tombstone). The slim tool
    /// description is unchanged — factual constants still ride it.
    Off,
    /// Status quo: index entry in the system prompt, body on demand via
    /// read_skill (D18).
    #[default]
    OnDemand,
    /// Assembly-time: the skill body is additionally injected into history as
    /// a synthetic tool_use/tool_result pair (fresh sessions only).
    Eager,
}

impl SkillInjection {
    /// Stable serde label for audit output (result.json / NDJSON anchors).
    pub fn as_str(self) -> &'static str {
        match self {
            SkillInjection::Off => "off",
            SkillInjection::OnDemand => "on_demand",
            SkillInjection::Eager => "eager",
        }
    }
}

/// `run_code` tool configuration (design docs/run_code_design.md §9). Plain
/// data living on every [`Config`] regardless of the `code` cargo feature, so
/// configs round-trip identically on builds without it — the tool itself only
/// exists behind `--features code`. The deny/fs lists and the call limit are
/// enforced by the M2 interception layer; the wall clock is the remaining M3
/// item.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodeConfig {
    /// Off by default (design D15): enabling `run_code` is an explicit trust
    /// decision — compiled in but not switched on, the model does NOT get
    /// "loop + code execution" as ambient capability.
    pub enabled: bool,
    /// Glob patterns (workdir-relative; `*` stays inside a segment, `**`
    /// crosses) that the 4 file tools refuse for guest-originated calls.
    /// Guards model MISREADING (e.g. "back up my env file"), not a hostile
    /// guest — shell passes `cat .env` straight through (design §7).
    pub deny_paths: Vec<String>,
    /// Empty = the whole workdir is writable by guest write_file/edit_file;
    /// otherwise only these dirs (wholesale replacement, not additive).
    pub fs_write_dirs: Vec<String>,
    /// 单次 run_code 执行的工具调用上限（每次调用独立计满，不跨次累计）。
    /// 默认 200（TinyAgentBench AppWorld 校准，2026-09）：真实批量任务的
    /// 金标方案在 91–244 次 API 调用区间，50 连"先摸元数据再批量写"的
    /// 两阶段打法都装不下（实测 agent 在摸底盘点阶段触顶、正事零执行）；
    /// 200 覆盖多数单阶段任务，更大的活由报错文案引导分批续跑。
    pub max_tool_calls: u32,
    pub mem_mb: u32,
    /// Wall-clock cap; 0 = none (fuel/memory still bound the run). Enforcement
    /// (epoch + entry checks) lands in M3 — parsed from M1.
    pub timeout_secs: u64,
    /// Deterministic-execution fuel budget (wasmtime fuel). M1 calibration:
    /// 1e9 ≈ 11.6s of pure interpreter loop on a debug build (~8.7e7 fuel/s;
    /// release faster). Default 5e9 (v2 P2 A/B evidence): a 2.5 MB readAll
    /// (117k lines → guest JSON parse + array build) exceeds 1e9, killing the
    /// documented "整读常见数据文件" path on first attempt — 5e9 covers it
    /// and stays well under the default timeout_secs=120.
    pub fuel: u64,
    /// Result cap in CHARS (same unit as `limits.max_output_chars`).
    pub max_result_chars: usize,
    /// Built-in `run-code` skill disclosure policy (finetune_support_design
    /// FR-1). Assembly-time decision: a runtime `set_code_config` replace does
    /// NOT re-inject or retract the skill (snapshot semantics, same precedent
    /// as other host hot-updates not propagating to constructed snapshots).
    pub skill_injection: SkillInjection,
    /// Guest pre-execution static precheck (finetune_support_design FR-2).
    /// Off by default (NFR-1: both finetune-support features require explicit
    /// configuration); when on, code matching known doomed patterns is
    /// rejected before Store instantiation with a precise message. An
    /// EFFICIENCY feature, NOT a security mechanism — lexical scans can be
    /// obfuscated past; the capability sandbox / deterministic stubs /
    /// interception layer do not depend on it.
    pub precheck: bool,
    /// Lib channel gate (code_memory_design MD-13, M1): enables the
    /// `__include`/`__hash_file` reserved names, the prelude `include()` /
    /// `saveLib()` helpers and the 【库】 description section. Off by default —
    /// the guest-visible contract of a gate-off binary is byte-identical to
    /// the pre-include build. Call-time via snapshot for the channels and
    /// prelude (resource-limit precedent); the description overlay is taken
    /// at `def()` time (skill_injection's snapshot precedent — a later
    /// `set_code_config` does not rewrite schemas already handed out).
    pub include: bool,
    /// Batch channel gate (code_llm_design LD-15, M2): enables the
    /// `__call_batch` reserved name, the prelude `tools.callBatch()` wrapper
    /// and the 【批量并发】 description/skill sections. Off by default — same
    /// gate-off contract discipline as `include` (A-L8). The batch surface is
    /// read-only by construction (`Tool::batchable`, LD-19): no atomicity,
    /// no rollback, partial execution is a normal outcome.
    pub call_batch: bool,
    /// The sandbox `llm` primitive's own config (code_llm_design §5, M3):
    /// gate + cost-tiered model routing. `enabled` is its own explicit trust
    /// decision (D15 class — NOT the sandbox's `enabled`); when on, the guest
    /// registry gains the `llm` tool (registry-build snapshot, LD-14) and
    /// `default_model`/`models` govern which models inner calls may reach.
    /// Validated eagerly by [`Config::validate_and_normalize`] so a broken
    /// enablement fails at config time, not at first sandbox call.
    #[serde(default)]
    pub llm: LlmConfig,
}

impl Default for CodeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            deny_paths: default_deny_paths(),
            fs_write_dirs: Vec::new(),
            max_tool_calls: 200,
            mem_mb: 256,
            timeout_secs: 120,
            fuel: 5_000_000_000,
            max_result_chars: 60_000,
            skill_injection: SkillInjection::OnDemand,
            precheck: false,
            include: false,
            call_batch: false,
            llm: LlmConfig::default(),
        }
    }
}

/// The sandbox `llm` primitive's config (code_llm_design §5, LD-5): a gate +
/// a cost-tiered model allowlist. Kept as plain data on `CodeConfig` (same
/// round-trip discipline: present regardless of the `code` cargo feature).
///
/// Invariants (enforced in [`Config::validate_and_normalize`], so both the
/// file-loading path and the Python binding's parse fail at config time):
/// - `enabled` requires `default_model` to be Some AND to appear in `models`
///   (a default the tier can't serve is a config typo, not a runtime event).
/// - `models` order is preserved as the declaration order; dedup happens at
///   parse normalization so the batch-overrides intersection logic and the
///   tool's allowlist never see duplicates.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    /// Off by default (D15 class): exposing inner model calls to guest code
    /// is its own explicit trust decision, independent of `code.enabled`.
    pub enabled: bool,
    /// The model used when a guest `llm` call omits `model` (LD-5). Required
    /// when enabled.
    pub default_model: Option<String>,
    /// The models an explicit `model` param may name (LD-5 allowlist).
    pub models: Vec<String>,
}

/// Default `deny_paths` (design §7): modern secret/credential shapes. Nested
/// `.aws` needs its own entry — `**/.aws/**` does not match the directory
/// itself at the workdir root.
fn default_deny_paths() -> Vec<String> {
    [
        ".env*",
        "**/.env*",
        "**/id_rsa*",
        "**/id_ed25519*",
        "**/id_ecdsa*",
        "**/*.pem",
        "**/.git/**",
        // linked worktrees carry `.git` as a FILE, not a directory
        "**/.git",
        "**/.git-credentials",
        "**/.netrc",
        ".aws/**",
        "**/.aws/**",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub provider: ProviderKind,
    /// Optional; falls back to the provider's default endpoint.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Name of the environment variable holding the API key.
    pub api_key_env: String,
    pub model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Task to execute in headless mode (overridable via --task).
    #[serde(default)]
    pub task: Option<String>,
    /// Working directory for all tool operations.
    #[serde(default)]
    pub workdir: Option<PathBuf>,
    /// Whitelist of enabled built-in tool names.
    #[serde(default = "default_tools")]
    pub tools: Vec<String>,
    /// When true, file tools reject paths outside `workdir`. Recommended
    /// for batch/CI runs with untrusted task files.
    #[serde(default)]
    pub restrict_to_workdir: bool,
    /// Prepend an `# Environment` block (OS / Shell / working directory) to the
    /// front of the system prompt at agent construction, so the model knows its
    /// runtime platform and emits compatible commands (e.g. it learns it is
    /// under `cmd /C`, not bash, and stops writing `$((...))` that silently
    /// no-ops and exits 0). Facts come from runtime/cfg — no syntax examples.
    /// Default on; set false to disable. Soft field (batch-overridable).
    #[serde(default = "default_inject_environment")]
    pub inject_environment: bool,
    /// When a reply is cut off at the output-token limit
    /// (`stop_reason: "max_tokens"`) with no pending tool calls, append one
    /// wrap-up user turn asking for a concise best-effort conclusion instead
    /// of returning a truncated (or, when reasoning consumed the whole
    /// budget, empty) reply. Bounded self-heal: at most one salvage turn per
    /// run — a salvage turn that also truncates is returned as-is; the
    /// salvage turn consumes turn budget and is billed like any other.
    /// Default on. Soft field (batch-overridable).
    #[serde(default = "default_salvage_on_max_tokens")]
    pub salvage_on_max_tokens: bool,
    #[serde(default)]
    pub limits: Limits,
    /// External MCP tool servers (stdio transport).
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// Reasoning effort forwarded to the provider verbatim as a string (e.g.
    /// "none" | "low" | "medium" | "high" | "xhigh" | "max"). NOT validated
    /// against an enum: every vendor's effort ladder differs and shifts across
    /// model generations (OpenAI minimal/none/low/medium/high/xhigh/max,
    /// Anthropic low/medium/high/max, GLM enabled/disabled, DeepSeek
    /// low/high/xhigh/max), so we forward the raw value and let the provider
    /// reject unknown ones. `None` = server default.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Host-registered absence notes for tool names this build deliberately
    /// does not carry: `(tool name, note)`. When the model calls such a name,
    /// dispatch returns the note instead of the bare "unknown tool: {name}"
    /// (see `ToolCtx::tool_absent_notes` for why the bare message harms).
    /// `assemble` copies this into the agent's `ToolCtx`, so every dispatch
    /// path — inline, parallel batch, run_code guest ctx, `task` children —
    /// relays the same note.
    #[serde(default)]
    pub tool_absent_notes: Vec<(String, String)>,
    /// `run_code` sandbox config (plain data — parses on any build; the tool
    /// itself requires the `code` cargo feature, design §9).
    #[serde(default)]
    pub code: CodeConfig,
}

/// Canonicalize + validate a workdir (must exist and be a directory). Shared
/// by `Config::load` and `Config::apply` (previously two hand-copied copies).
fn resolve_workdir(dir: PathBuf) -> Result<PathBuf> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("workdir does not exist: {}", dir.display()))?;
    if !dir.is_dir() {
        bail!("workdir is not a directory: {}", dir.display());
    }
    Ok(dir)
}

/// `Config::api_key`'s failure copy, split by `VarError` variant (P2): the
/// old single `with_context` reported BOTH failures as "environment
/// variable X is not set", so a variable that IS set but holds non-UTF-8
/// bytes (Windows-wide env, locale-mangled shell) sent the operator to
/// re-export a key that exists instead of fixing its encoding — the two
/// failure modes call for opposite first moves. `NotUnicode`'s `OsString`
/// payload is the (secret, unprintable) value itself and is deliberately
/// dropped, never embedded in the message. A free function over the
/// pre-fetched error so the NotUnicode branch is unit-testable:
/// `std::env::var` reads the live process environment and no test API can
/// inject a non-UTF-8 value into it, so through `api_key()` itself only
/// the NotPresent arm is reachable end-to-end.
fn api_key_env_error(var: &str, e: std::env::VarError) -> anyhow::Error {
    match e {
        std::env::VarError::NotPresent => {
            anyhow::anyhow!("environment variable {var} is not set")
        }
        std::env::VarError::NotUnicode(_) => {
            anyhow::anyhow!("environment variable {var} is set, but its value is not valid UTF-8")
        }
    }
}

/// String-level twin of `tools::validate_tool_names` for the config's
/// built-in whitelist: `Config::tools` holds NAMES, not constructed tools —
/// the Tool-object validator runs later at assembly over the resolved list
/// (built-in + MCP + skill + extra). Same rules, so an overlay caught here
/// fails with the overlay blamed instead of surfacing at assembly/provider
/// time: non-empty, `[A-Za-z0-9_-]` only, ≤64 chars (the Anthropic
/// tool-name / OpenAI function-name wire limit), unique. The reserved-name
/// checks (`__tools_list`, sandbox `llm`) are deliberately absent here: they
/// guard CONSTRUCTED tools (MCP/custom), and a whitelist string naming one
/// is already rejected as an unknown built-in at assembly.
fn validate_tool_whitelist(names: &[String]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for name in names {
        if name.is_empty() {
            bail!("tool name must not be empty");
        }
        if let Some(bad) = name
            .chars()
            .find(|c| !c.is_ascii_alphanumeric() && *c != '_' && *c != '-')
        {
            bail!(
                "tool name '{name}' contains '{bad}': names may only use \
                 [A-Za-z0-9_-] (the Anthropic/OpenAI tool-name character set)"
            );
        }
        if name.chars().count() > 64 {
            bail!(
                "tool name '{name}' exceeds the 64-char provider limit \
                 (Anthropic tool name / OpenAI function name)"
            );
        }
        if !seen.insert(name.clone()) {
            bail!(
                "duplicate tool name '{name}' in the tools whitelist — the \
                 duplicate would be silently shadowed at assembly (first \
                 match wins at dispatch)"
            );
        }
    }
    Ok(())
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config file: {}", path.display()))?;
        let mut cfg: Config = serde_json::from_str(&raw)
            .with_context(|| format!("invalid config JSON: {}", path.display()))?;
        cfg.validate_and_normalize()?;
        Ok(cfg)
    }

    /// Post-deserialization normalization + validation, shared by EVERY config
    /// entry point so they cannot drift: file load ([`Config::load`]) and the
    /// Python binding's inline dict both route through here. Normalizes the
    /// configured `workdir` (canonicalize + is-dir check — the sandbox root,
    /// skill discovery, and result paths all derive from it) and validates MCP
    /// server entries eagerly (a bad server fails at config time with the
    /// config blamed, not at first connect with a confusing transport error).
    pub fn validate_and_normalize(&mut self) -> Result<()> {
        for s in &self.mcp_servers {
            s.validate()?;
        }
        self.workdir = self.workdir.take().map(resolve_workdir).transpose()?;
        // `code.llm` enablement invariants (code_llm_design §5, LD-5): fail
        // at config time with the config blamed, not at the first sandbox
        // `llm` call with a confusing provider error. Every config entry
        // point (file load, Python inline dict) routes through here.
        self.code.normalize_and_validate_llm()?;
        Ok(())
    }

    pub fn base_url(&self) -> String {
        let base = self
            .base_url
            .clone()
            .unwrap_or_else(|| self.provider.default_base_url().to_string());
        // Trim trailing slashes: providers join with `format!("{}/path")`,
        // so a config ending in "/" would request `...v1//chat/completions`
        // — some gateways 404 on the doubled slash.
        base.trim_end_matches('/').to_string()
    }

    pub fn api_key(&self) -> Result<String> {
        std::env::var(&self.api_key_env).map_err(|e| api_key_env_error(&self.api_key_env, e))
    }

    /// Overlay a partial config fragment (batch mode) — the UNTRUSTED entry:
    /// only soft fields are applied (task / system_prompt / model /
    /// max_tokens / reasoning_effort / inject_environment /
    /// salvage_on_max_tokens / limits / `code` under its clamps); any hard
    /// field (provider / base_url / api_key_env / workdir / tools /
    /// restrict_to_workdir / mcp_servers) is REJECTED here with an error
    /// pointing at [`Self::apply_trusted`].
    ///
    /// Since the bin/lib split this method is a published library API. The
    /// hard-field screen used to live only in the CALLER
    /// ([`PartialConfig::forbidden_batch_override`] — "check-then-apply" by
    /// discipline, fine while the CLI was the sole caller), but a
    /// third-party host feeding a raw task-file fragment straight to `apply`
    /// handed that fragment the power to move the endpoint, swap the API-key
    /// variable, rewrite the tool whitelist and inject unvalidated MCP
    /// servers. The guard now lives IN the method; the CLI still runs
    /// `forbidden_batch_override` first because its message is the better
    /// bad_task copy (it names the whole overridable set).
    pub fn apply(&mut self, p: PartialConfig) -> Result<()> {
        if let Some(field) = p.forbidden_batch_override() {
            bail!(
                "config fragment must not set '{field}' (hard field: endpoints, \
                 credentials, workdir, tool whitelist and MCP servers come from \
                 the trusted base config); if this overlay is host-constructed \
                 and already screened, use apply_trusted — see \
                 PartialConfig::forbidden_batch_override"
            );
        }
        self.apply_trusted(p)
    }

    /// [`Self::apply`] without the untrusted-fragment screen: applies EVERY
    /// field, hard ones included. ONLY for overlays the HOST itself
    /// constructed and already vetted — the CLI batch path (its fragments
    /// are pre-screened by [`PartialConfig::forbidden_batch_override`],
    /// which produces the better error copy). Never feed this a raw
    /// task-file fragment: it can move endpoints, swap `api_key_env`,
    /// rewrite the tool whitelist and inject MCP servers. As partial
    /// backstops, the two validations the old unchecked path was missing are
    /// enforced HERE rather than left to later entry points: `mcp_servers`
    /// entries are `validate()`d eagerly (same promise as [`Self::load`]:
    /// fail at overlay time with the overlay blamed, not at first connect
    /// with a confusing transport error) and `tools` names are checked with
    /// the same rules as agent construction (`validate_tool_whitelist`).
    ///
    /// The `PartialConfig` is destructured WITHOUT `..` at the top: adding a
    /// field to it without classifying it here (soft overlay) AND in
    /// `forbidden_batch_override` (hard/soft) is a compile error — closing
    /// the historical "field added, wiring forgotten, override silently
    /// dropped" hole (and its security-flavored sibling: a hard field
    /// forgotten from the forbidden list) at the type level, not by
    /// discipline. Never reintroduce `..` in these destructures.
    pub fn apply_trusted(&mut self, p: PartialConfig) -> Result<()> {
        let PartialConfig {
            provider,
            base_url,
            api_key_env,
            model,
            max_tokens,
            system_prompt,
            task,
            workdir,
            tools,
            restrict_to_workdir,
            limits,
            mcp_servers,
            reasoning_effort,
            inject_environment,
            salvage_on_max_tokens,
            code,
        } = p;
        if let Some(v) = provider {
            self.provider = v;
        }
        if base_url.is_some() {
            self.base_url = base_url;
        }
        if let Some(v) = api_key_env {
            self.api_key_env = v;
        }
        if let Some(v) = model {
            self.model = v;
        }
        if let Some(v) = max_tokens {
            self.max_tokens = v;
        }
        if system_prompt.is_some() {
            self.system_prompt = system_prompt;
        }
        if task.is_some() {
            self.task = task;
        }
        if let Some(dir) = workdir {
            self.workdir = Some(resolve_workdir(dir)?);
        }
        if let Some(v) = tools {
            // Same rules as agent construction (see validate_tool_whitelist):
            // a bad name would otherwise only fail at assembly or as a
            // provider 400, far from the overlay that introduced it.
            validate_tool_whitelist(&v)?;
            self.tools = v;
        }
        if let Some(v) = restrict_to_workdir {
            self.restrict_to_workdir = v;
        }
        if reasoning_effort.is_some() {
            self.reasoning_effort = reasoning_effort;
        }
        if let Some(v) = inject_environment {
            self.inject_environment = v;
        }
        if let Some(v) = salvage_on_max_tokens {
            self.salvage_on_max_tokens = v;
        }
        if let Some(l) = limits {
            // Same exhaustive-destructure discipline as above: a new
            // PartialLimits field that is not classified here is a compile
            // error, not a silently dropped override. Each field's declared
            // `Clamp` policy is the single source of what a batch task
            // fragment may do to it (batch task files are untrusted).
            let PartialLimits {
                max_turns,
                timeout_secs,
                max_output_chars,
                compress_threshold_chars,
                keep_recent_messages,
                max_context_tokens,
                max_total_tokens,
                stream_idle_secs,
                shell_timeout_secs,
                subagent_max_turns,
            } = l;
            let li = &mut self.limits;
            li.max_turns = Clamp::Tighten.merge(li.max_turns, max_turns);
            li.timeout_secs = Clamp::TightenOrInit.merge(li.timeout_secs, timeout_secs);
            li.max_output_chars = Clamp::Tighten.merge(li.max_output_chars, max_output_chars);
            li.compress_threshold_chars =
                Clamp::TightenOrInit.merge(li.compress_threshold_chars, compress_threshold_chars);
            li.keep_recent_messages =
                Clamp::Tighten.merge(li.keep_recent_messages, keep_recent_messages);
            li.max_context_tokens =
                Clamp::TightenOrInit.merge(li.max_context_tokens, max_context_tokens);
            li.max_total_tokens = Clamp::TightenOrInit.merge(li.max_total_tokens, max_total_tokens);
            li.stream_idle_secs = Clamp::Tighten.merge(li.stream_idle_secs, stream_idle_secs);
            // The only Replace: cost/latency knob, not a safety boundary.
            li.shell_timeout_secs = Clamp::Replace.merge(li.shell_timeout_secs, shell_timeout_secs);
            // 0 = no separate ceiling (loosest), so a fragment may only tighten.
            li.subagent_max_turns =
                Clamp::TightenOrInit.merge(li.subagent_max_turns, subagent_max_turns);
        }
        if let Some(v) = mcp_servers {
            // Eager validation, same promise as Config::load: the old apply
            // never validated here (it leaned on the caller-side screen, which
            // a library host may skip) — a bad server must fail at overlay
            // time with the overlay blamed, not at first connect.
            for s in &v {
                s.validate()?;
            }
            self.mcp_servers = v;
        }
        if let Some(pc) = code {
            if pc.enabled.is_some() {
                anyhow::bail!(
                    "task file must not set code.enabled — untrusted fragments \
                     cannot switch the sandbox on or off"
                );
            }
            if let Some(pl) = &pc.llm {
                if pl.enabled.is_some() {
                    anyhow::bail!(
                        "task file must not set code.llm.enabled — untrusted \
                         fragments cannot switch the inner-model primitive on \
                         or off"
                    );
                }
                if pl.default_model.is_some() {
                    anyhow::bail!(
                        "task file must not set code.llm.default_model — the \
                         default route is the trusted base config's decision; \
                         fragments may only narrow code.llm.models"
                    );
                }
            }
            self.code.merge_partial(pc)?;
        }
        Ok(())
    }
}

/// All-optional config fragment used by batch task files.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialConfig {
    pub provider: Option<ProviderKind>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
    pub system_prompt: Option<String>,
    pub task: Option<String>,
    pub workdir: Option<PathBuf>,
    pub tools: Option<Vec<String>>,
    pub restrict_to_workdir: Option<bool>,
    pub limits: Option<PartialLimits>,
    pub mcp_servers: Option<Vec<McpServerConfig>>,
    pub reasoning_effort: Option<String>,
    pub inject_environment: Option<bool>,
    pub salvage_on_max_tokens: Option<bool>,
    pub code: Option<PartialCodeConfig>,
}

impl PartialConfig {
    /// First security-relevant field this fragment tries to set, if any.
    /// Batch task files are untrusted: they may only override soft fields
    /// (task / system_prompt / model / max_tokens / reasoning_effort /
    /// inject_environment / limits). Everything else (endpoints, credentials,
    /// working directory, tool whitelist, sandbox, MCP servers) must come
    /// from the trusted base config and must not be weakened per task.
    ///
    /// [`Config::apply`] enforces the SAME list in-method (defense for
    /// library hosts that skip this check), so running it first is about
    /// error COPY, not correctness: this method names the field in the
    /// CLI's bad_task message. Hosts constructing a vetted overlay that
    /// legitimately sets hard fields use [`Config::apply_trusted`].
    ///
    /// The destructure below lists EVERY field exactly once — soft ones as
    /// `field: _` (explicitly allowed), hard ones bound and checked in
    /// reporting order. Adding a field without picking a side is a compile
    /// error; never reintroduce `..` here.
    pub fn forbidden_batch_override(&self) -> Option<&'static str> {
        let PartialConfig {
            provider,
            base_url,
            api_key_env,
            model: _,
            max_tokens: _,
            system_prompt: _,
            task: _,
            workdir,
            tools,
            restrict_to_workdir,
            limits: _,
            mcp_servers,
            reasoning_effort: _,
            inject_environment: _,
            salvage_on_max_tokens: _,
            // `code` is soft: its sub-fields each have a clamp/merge policy
            // in CodeConfig::merge_partial (§9), and the one that isn't
            // (enabled) is rejected by Config::apply → bad_task.
            code: _,
        } = self;
        if provider.is_some() {
            return Some("provider");
        }
        if base_url.is_some() {
            return Some("base_url");
        }
        if api_key_env.is_some() {
            return Some("api_key_env");
        }
        if workdir.is_some() {
            return Some("workdir");
        }
        if tools.is_some() {
            return Some("tools");
        }
        if restrict_to_workdir.is_some() {
            return Some("restrict_to_workdir");
        }
        if mcp_servers.is_some() {
            return Some("mcp_servers");
        }
        None
    }
}

/// Batch-overlay shape for [`CodeConfig`] (design §9): every field optional.
/// The merge is per-field — an untrusted fragment can only TIGHTEN — and
/// `enabled` never reaches the merge (Config::apply rejects its presence
/// outright: untrusted input may not switch the sandbox on or off).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialCodeConfig {
    pub enabled: Option<bool>,
    pub deny_paths: Option<Vec<String>>,
    pub fs_write_dirs: Option<Vec<String>>,
    pub max_tool_calls: Option<u32>,
    pub mem_mb: Option<u32>,
    pub timeout_secs: Option<u64>,
    pub fuel: Option<u64>,
    pub max_result_chars: Option<usize>,
    pub skill_injection: Option<SkillInjection>,
    pub precheck: Option<bool>,
    pub include: Option<bool>,
    pub call_batch: Option<bool>,
    pub llm: Option<PartialLlmConfig>,
}

/// Batch-overlay shape for [`LlmConfig`]: the fragment may only SHRINK the
/// tier (`models` intersection in `CodeConfig::merge_partial`). `enabled`
/// and `default_model` are rejected outright by `Config::apply` — switching
/// the primitive on/off and picking the default route are the trusted base
/// config's calls, same class as `code.enabled`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialLlmConfig {
    pub enabled: Option<bool>,
    pub default_model: Option<String>,
    pub models: Option<Vec<String>>,
}

impl CodeConfig {
    /// `code.llm` normalization + enablement invariants (code_llm_design §5,
    /// LD-5): stable first-occurrence dedup, then — when enabled — non-empty
    /// allowlist and `default_model ∈ models`. Shared by EVERY trusted
    /// config entrance: file load / Python inline dict (via
    /// [`Config::validate_and_normalize`]) and the runtime
    /// `Agent::set_code_config` hot replace (external audit P2-5: the setter
    /// used to bypass this, letting a default outside the allowlist dispatch
    /// real omitted-model calls).
    pub(crate) fn normalize_and_validate_llm(&mut self) -> Result<()> {
        // Stable first-occurrence dedup (plain `dedup` only collapses
        // ADJACENT copies): declaration order is preserved for the
        // batch-overlays intersection and the tool's allowlist never sees
        // duplicates.
        {
            let mut seen = std::collections::HashSet::new();
            self.llm.models.retain(|m| seen.insert(m.clone()));
        }
        if self.llm.enabled {
            if self.llm.models.is_empty() {
                bail!(
                    "code.llm.enabled requires a non-empty code.llm.models \
                     allowlist (the cost tier guest `llm` calls may route within)"
                );
            }
            match &self.llm.default_model {
                None => bail!(
                    "code.llm.enabled requires code.llm.default_model (the \
                     model used when a guest llm call omits `model`)"
                ),
                Some(m) if !self.llm.models.contains(m) => bail!(
                    "code.llm.default_model `{}` must appear in code.llm.models \
                     — a default the tier cannot serve is a config typo, not a \
                     runtime event",
                    m
                ),
                _ => {}
            }
        }
        Ok(())
    }

    /// §9 merge table. Exhaustive destructure (no `..`): a field added to
    /// [`PartialCodeConfig`] without a policy here is a compile error — the
    /// same anti-fossil discipline as `PartialLimits`. Fallsible only for
    /// the `llm` post-condition (an overlay that breaks an enabled tier's
    /// default route is a bad_task, not a silently-broken config).
    fn merge_partial(&mut self, p: PartialCodeConfig) -> Result<()> {
        let PartialCodeConfig {
            enabled: _, // rejected by Config::apply before we get here
            deny_paths,
            fs_write_dirs,
            max_tool_calls,
            mem_mb,
            timeout_secs,
            fuel,
            max_result_chars,
            skill_injection,
            precheck,
            include,
            call_batch,
            llm,
        } = p;
        if let Some(v) = deny_paths {
            // Union — a fragment can only ADD denials. Sort+dedup (plain
            // `dedup` only collapses ADJACENT copies, so a duplicate added by
            // a fragment would survive it); order is semantically inert for a
            // match list and sorted output keeps configs diffable.
            self.deny_paths.extend(v);
            self.deny_paths.sort();
            self.deny_paths.dedup();
        }
        if let Some(v) = fs_write_dirs {
            // Intersection with EMPTY = "whole workdir writable" as the
            // universe element (中-6: semantics must not flip based on
            // whether the value came from the default or an explicit
            // config): either side empty → the other side; both non-empty →
            // entry-wise intersection (may be empty — tightened to
            // nowhere-writable, a legal end state).
            if self.fs_write_dirs.is_empty() {
                self.fs_write_dirs = v;
            } else if !v.is_empty() {
                let frag: std::collections::HashSet<_> = v.iter().collect();
                self.fs_write_dirs.retain(|d| frag.contains(d));
            }
        }
        self.max_tool_calls = Clamp::Tighten.merge(self.max_tool_calls, max_tool_calls);
        self.mem_mb = Clamp::Tighten.merge(self.mem_mb, mem_mb);
        self.timeout_secs = Clamp::TightenOrInit.merge(self.timeout_secs, timeout_secs);
        self.fuel = Clamp::Tighten.merge(self.fuel, fuel);
        self.max_result_chars = Clamp::Tighten.merge(self.max_result_chars, max_result_chars);
        // Disclosure/intervention knobs: a fragment may only narrow them —
        // min under the declaration ordering Off < OnDemand < Eager (FD-2) and
        // false < true for precheck. A fragment asking to disclose MORE than
        // the trusted base is silently clamped to the base.
        self.skill_injection = Clamp::Tighten.merge(self.skill_injection, skill_injection);
        self.precheck = Clamp::Tighten.merge(self.precheck, precheck);
        // Lib channel gate: Tighten (MD-13) — an untrusted fragment may only
        // turn include OFF; enabling the channel is the trusted base config's
        // call (same class as precheck, a host-side capability switch).
        self.include = Clamp::Tighten.merge(self.include, include);
        // Batch channel gate: Tighten (LD-15) — same rule, same class.
        self.call_batch = Clamp::Tighten.merge(self.call_batch, call_batch);
        // llm: `enabled`/`default_model` presence is rejected by
        // Config::apply before we get here; the fragment's only power over
        // the tier is REMOVING models.
        if let Some(PartialLlmConfig {
            enabled: _,
            default_model: _,
            models,
        }) = llm
        {
            if let Some(v) = models {
                // Intersection, base declaration order preserved: a fragment
                // can only tighten. Unlike fs_write_dirs, EMPTY here means
                // "nothing allowed", not the universe — an empty side just
                // yields an empty tier.
                let frag: std::collections::HashSet<_> = v.iter().collect();
                self.llm.models.retain(|m| frag.contains(m));
            }
            // Post-condition (LD-5): an enabled primitive must still be able
            // to serve its default after the narrowing. A base that was
            // validated keeps this true for every overlay that doesn't drop
            // the default, so reaching the failure means the fragment DID
            // drop it — bad_task with an actionable message.
            if self.llm.enabled {
                if let Some(m) = &self.llm.default_model {
                    if !self.llm.models.contains(m) {
                        bail!(
                            "code.llm.models overlay drops default_model `{}` \
                             — the narrowed tier cannot serve the default model",
                            m
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialLimits {
    pub max_turns: Option<u32>,
    pub timeout_secs: Option<u64>,
    pub max_output_chars: Option<usize>,
    pub compress_threshold_chars: Option<usize>,
    pub keep_recent_messages: Option<usize>,
    pub max_context_tokens: Option<u64>,
    pub max_total_tokens: Option<u64>,
    pub stream_idle_secs: Option<u64>,
    pub shell_timeout_secs: Option<u64>,
    pub subagent_max_turns: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_and_normalize_resolves_and_rejects_workdir() {
        // The shared post-deserialization step (file load + the Python binding's
        // inline dicts): a real workdir is canonicalized (symlinks resolved —
        // the sandbox root and confinement checks derive from it), a
        // nonexistent one fails HERE with the config blamed, instead of
        // surfacing later at first tool use with a confusing error.
        let mut cfg: Config =
            serde_json::from_str(r#"{"provider":"openai","api_key_env":"K","model":"m"}"#).unwrap();
        let tmp = std::env::temp_dir(); // may itself sit behind a symlink (/tmp)
        cfg.workdir = Some(tmp.clone());
        cfg.validate_and_normalize().unwrap();
        assert_eq!(
            cfg.workdir.as_deref(),
            Some(tmp.canonicalize().unwrap().as_path()),
            "workdir is canonicalized"
        );
        cfg.workdir = Some(tmp.join("definitely-not-a-real-dir-for-config-tests"));
        let err = cfg.validate_and_normalize().unwrap_err();
        assert!(format!("{err:#}").contains("workdir"), "{err:#}");
    }

    #[test]
    fn parse_minimal_config() {
        let json = r#"{
            "provider": "anthropic",
            "api_key_env": "ANTHROPIC_API_KEY",
            "model": "claude-haiku-4-5-20251001"
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.provider, ProviderKind::Anthropic);
        assert_eq!(cfg.base_url(), "https://api.anthropic.com");
        assert_eq!(cfg.limits.max_turns, 30);
        assert_eq!(
            cfg.tools,
            vec![
                "shell",
                "read_file",
                "write_file",
                "edit_file",
                "list_dir",
                "task",
                "todo"
            ]
        );
    }

    #[test]
    fn parse_full_config() {
        let json = r#"{
            "provider": "openai",
            "base_url": "http://localhost:8000",
            "api_key_env": "OPENAI_API_KEY",
            "model": "qwen3-4b",
            "max_tokens": 4096,
            "task": "run tests",
            "tools": ["shell"],
            "limits": {"max_turns": 5, "timeout_secs": 60, "max_output_chars": 1000}
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.provider, ProviderKind::Openai);
        assert_eq!(cfg.base_url(), "http://localhost:8000");
        assert_eq!(cfg.limits.max_output_chars, 1000);
        assert_eq!(cfg.tools, vec!["shell"]);
    }

    #[test]
    fn base_url_trims_trailing_slashes() {
        // Providers join with `format!("{}/path")`; a config ending in "/"
        // would request `...v1//chat/completions` — some gateways 404 on the
        // doubled slash. Multiple slashes collapse too (typo "v1//").
        let json = r#"{
            "provider": "openai",
            "base_url": "http://localhost:8000/v1//",
            "api_key_env": "OPENAI_API_KEY",
            "model": "qwen3-4b"
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.base_url(), "http://localhost:8000/v1");
    }

    /// P2: `api_key()`'s failure copy must split by `VarError` variant — the
    /// old blanket `with_context` reported a set-but-non-UTF-8 variable as
    /// "not set", sending the operator to re-export a key that exists. The
    /// helper takes the PRE-FETCHED error precisely so the NotUnicode branch
    /// is drivable: `env::var` reads the live process environment and no
    /// test API can inject a non-UTF-8 value, so through `api_key()` itself
    /// only NotPresent is reachable end-to-end (asserted below with a
    /// definitely-unset unique name; the bin's batch setup_error tests cover
    /// the same path at the harness level). NotUnicode's OsString payload is
    /// the secret itself — the messages must carry the variable NAME only.
    #[test]
    fn api_key_error_copy_splits_missing_from_non_utf8_environment_value() {
        // NotPresent: the exact historical wording, kept for every consumer
        // that greps logs/messages for it.
        let not_set = api_key_env_error("MY_KEY", std::env::VarError::NotPresent).to_string();
        assert_eq!(not_set, "environment variable MY_KEY is not set");

        // NotUnicode: says SET (not missing) and names the encoding problem.
        let not_utf8 = api_key_env_error(
            "MY_KEY",
            std::env::VarError::NotUnicode(std::ffi::OsString::new()),
        )
        .to_string();
        assert!(
            not_utf8.contains("MY_KEY")
                && not_utf8.contains("is set")
                && not_utf8.to_lowercase().contains("utf-8"),
            "NotUnicode must be reported as set-but-not-UTF-8: {not_utf8}"
        );
        assert!(
            !not_utf8.contains("not set"),
            "the misleading NotPresent copy must not be reused: {not_utf8}"
        );

        // End-to-end NotPresent wiring through api_key() itself (unique var
        // name so parallel tests in this process cannot observe each other).
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"TA_CFG_TEST_DEFINITELY_UNSET_KEY","model":"m"}"#,
        )
        .unwrap();
        let err = cfg.api_key().unwrap_err().to_string();
        assert_eq!(
            err,
            "environment variable TA_CFG_TEST_DEFINITELY_UNSET_KEY is not set"
        );
    }

    #[test]
    fn partial_config_overlay() {
        let json = r#"{
            "provider": "anthropic",
            "api_key_env": "ANTHROPIC_API_KEY",
            "model": "base-model",
            "tools": ["shell"],
            "limits": {"max_turns": 10, "timeout_secs": 100, "max_output_chars": 500}
        }"#;
        let mut cfg: Config = serde_json::from_str(json).unwrap();
        let partial: PartialConfig = serde_json::from_str(
            r#"{"task": "do x", "limits": {"max_turns": 3}, "model": "task-model"}"#,
        )
        .unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(cfg.model, "task-model");
        assert_eq!(cfg.task.as_deref(), Some("do x"));
        assert_eq!(cfg.limits.max_turns, 3);
        assert_eq!(cfg.limits.timeout_secs, 100); // untouched
        assert_eq!(cfg.tools, vec!["shell"]); // untouched
    }

    #[test]
    fn reasoning_effort_and_mcp_env_parse() {
        // reasoning_effort is an optional string, passed through verbatim (no
        // enum validation). env on a stdio MCP server is a string→string map.
        let json = r#"{
            "provider": "openai", "api_key_env": "K", "model": "m",
            "reasoning_effort": "xhigh",
            "mcp_servers": [{"name":"fs","command":"npx","env":{"FOO":"bar","BAZ":"qux"}}]
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("xhigh"));
        assert_eq!(
            cfg.mcp_servers[0].env.get("FOO").map(String::as_str),
            Some("bar")
        );
        assert_eq!(
            cfg.mcp_servers[0].env.get("BAZ").map(String::as_str),
            Some("qux")
        );
    }

    #[test]
    fn partial_config_overlays_reasoning_effort_without_being_forbidden() {
        // reasoning_effort is a soft field like model/max_tokens: a batch task
        // fragment may override it, and it is NOT in forbidden_batch_override.
        let mut cfg: Config =
            serde_json::from_str(r#"{"provider":"anthropic","api_key_env":"K","model":"m"}"#)
                .unwrap();
        let partial: PartialConfig = serde_json::from_str(r#"{"reasoning_effort":"low"}"#).unwrap();
        assert!(partial.forbidden_batch_override().is_none());
        cfg.apply(partial).unwrap();
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("low"));
    }

    #[test]
    fn partial_config_overlays_inject_environment_without_being_forbidden() {
        // inject_environment is a soft field: a batch task fragment may override
        // it (e.g. force the env block off for a locked eval), and it is NOT in
        // forbidden_batch_override.
        let mut cfg: Config =
            serde_json::from_str(r#"{"provider":"anthropic","api_key_env":"K","model":"m"}"#)
                .unwrap();
        assert!(cfg.inject_environment, "defaults true");
        let partial: PartialConfig =
            serde_json::from_str(r#"{"inject_environment":false}"#).unwrap();
        assert!(partial.forbidden_batch_override().is_none());
        cfg.apply(partial).unwrap();
        assert!(!cfg.inject_environment, "overlaid to false");
    }

    #[test]
    fn mcp_server_validation() {
        let both: McpServerConfig =
            serde_json::from_str(r#"{"name":"x","command":"npx","url":"http://h/mcp"}"#).unwrap();
        assert!(both.validate().is_err());

        let neither: McpServerConfig = serde_json::from_str(r#"{"name":"x"}"#).unwrap();
        assert!(neither.validate().is_err());

        let stdio_with_headers: McpServerConfig =
            serde_json::from_str(r#"{"name":"x","command":"npx","headers":{"A":"b"}}"#).unwrap();
        assert!(stdio_with_headers.validate().is_err());

        let http: McpServerConfig = serde_json::from_str(
            r#"{"name":"x","url":"http://h/mcp","transport":"http",
                "headers":{"Authorization":"Bearer t"}}"#,
        )
        .unwrap();
        assert!(http.validate().is_ok());
        assert_eq!(http.transport, Some(McpTransport::Http));
    }

    #[test]
    fn batch_overlay_rejects_unsafe_fields() {
        // Soft fields are fine.
        let ok: PartialConfig =
            serde_json::from_str(r#"{"task":"x","model":"m","limits":{"max_turns":3}}"#).unwrap();
        assert!(ok.forbidden_batch_override().is_none());

        // Security-relevant fields are refused.
        for (json, field) in [
            (r#"{"base_url":"http://x"}"#, "base_url"),
            (r#"{"api_key_env":"X"}"#, "api_key_env"),
            (r#"{"tools":["shell"]}"#, "tools"),
            (r#"{"restrict_to_workdir":false}"#, "restrict_to_workdir"),
            (r#"{"provider":"openai"}"#, "provider"),
            (r#"{"workdir":"/"}"#, "workdir"),
        ] {
            let p: PartialConfig = serde_json::from_str(json).unwrap();
            assert_eq!(p.forbidden_batch_override(), Some(field), "{json}");
        }
    }

    #[test]
    fn config_rejects_unknown_field() {
        // deny_unknown_fields: a typo like "timeout_sec" must fail loudly
        // instead of silently falling back to the default.
        let json = r#"{
            "provider": "anthropic",
            "api_key_env": "K",
            "model": "m",
            "limits": {"timeout_sec": 60}
        }"#;
        assert!(serde_json::from_str::<Config>(json).is_err());
    }

    #[test]
    fn batch_overlay_clamps_limits_to_base() {
        // A task can only tighten limits, never loosen them.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"max_turns":10,"timeout_secs":60,"max_output_chars":5000}}"#,
        )
        .unwrap();
        let partial: PartialConfig = serde_json::from_str(
            r#"{"task":"x","limits":{"max_turns":100,"timeout_secs":0,"max_output_chars":99999}}"#,
        )
        .unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(cfg.limits.max_turns, 10); // clamped down to base
        assert_eq!(cfg.limits.timeout_secs, 60); // 0 (no-limit) ignored
        assert_eq!(cfg.limits.max_output_chars, 5000); // clamped

        // Tightening is allowed.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"max_turns":10,"timeout_secs":60}}"#,
        )
        .unwrap();
        let partial: PartialConfig =
            serde_json::from_str(r#"{"task":"x","limits":{"max_turns":3,"timeout_secs":30}}"#)
                .unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(cfg.limits.max_turns, 3);
        assert_eq!(cfg.limits.timeout_secs, 30);
    }

    #[test]
    fn batch_overlay_timeout_tightens_from_no_limit_base() {
        // timeout_secs==0 is the LOOSEST value ("no limit"), unlike every other
        // field where 0 is tightest. A task setting timeout_secs=30 over a base
        // of 0 must tighten to 30 — NOT collapse back to 0 via v.min(0), which
        // would silently drop the tightening and leave the worker unbounded.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"timeout_secs":0}}"#,
        )
        .unwrap();
        let partial: PartialConfig =
            serde_json::from_str(r#"{"task":"x","limits":{"timeout_secs":30}}"#).unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(
            cfg.limits.timeout_secs, 30,
            "0-base must adopt the finite task value"
        );

        // From a finite base the task can still only tighten (min), never loosen.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"timeout_secs":60}}"#,
        )
        .unwrap();
        let partial: PartialConfig =
            serde_json::from_str(r#"{"task":"x","limits":{"timeout_secs":120}}"#).unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(
            cfg.limits.timeout_secs, 60,
            "a larger task timeout must not loosen the base"
        );
    }

    #[test]
    fn batch_overlay_max_context_tokens_only_tightens() {
        // max_context_tokens is 0-sentineled: 0 means "no preflight token
        // compaction" (loosest). A task may set it (tighten: compact sooner) or
        // tighten a finite base, but may NOT loosen/disable it.
        // 0 in the partial is ignored — it's the no-op sentinel.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"max_context_tokens":200000}}"#,
        )
        .unwrap();
        let partial: PartialConfig =
            serde_json::from_str(r#"{"task":"x","limits":{"max_context_tokens":0}}"#).unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(
            cfg.limits.max_context_tokens, 200000,
            "0 sentinel must not disable"
        );

        // From a disabled base (0), a finite task value is adopted.
        let mut cfg: Config =
            serde_json::from_str(r#"{"provider":"anthropic","api_key_env":"K","model":"m"}"#)
                .unwrap();
        assert_eq!(cfg.limits.max_context_tokens, 0);
        let partial: PartialConfig =
            serde_json::from_str(r#"{"task":"x","limits":{"max_context_tokens":8000}}"#).unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(
            cfg.limits.max_context_tokens, 8000,
            "finite value adopted from 0 base"
        );

        // From a finite base, a task can only tighten (min), never loosen.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"max_context_tokens":100000}}"#,
        )
        .unwrap();
        let partial: PartialConfig =
            serde_json::from_str(r#"{"task":"x","limits":{"max_context_tokens":50000}}"#).unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(cfg.limits.max_context_tokens, 50000);

        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"max_context_tokens":100000}}"#,
        )
        .unwrap();
        let partial: PartialConfig =
            serde_json::from_str(r#"{"task":"x","limits":{"max_context_tokens":200000}}"#).unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(
            cfg.limits.max_context_tokens, 100000,
            "larger task value must not loosen"
        );
    }

    #[test]
    fn batch_overlay_compress_threshold_0_sentinel_only_tightens() {
        // compress_threshold_chars is 0-sentineled like max_context_tokens: 0
        // means "no preflight compression" (loosest). A task must NOT be able to
        // disable the base's compaction by setting 0 (min(base,0)==0 would).
        // 0 in the partial is the no-op sentinel and is ignored.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"compress_threshold_chars":100000}}"#,
        )
        .unwrap();
        let partial: PartialConfig =
            serde_json::from_str(r#"{"task":"x","limits":{"compress_threshold_chars":0}}"#)
                .unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(
            cfg.limits.compress_threshold_chars, 100000,
            "0 sentinel must not disable the base's preflight compaction"
        );

        // From a disabled base (0), a finite task value is adopted (enables it).
        let cfg_disabled: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"compress_threshold_chars":0}}"#,
        )
        .unwrap();
        assert_eq!(cfg_disabled.limits.compress_threshold_chars, 0);
        let partial: PartialConfig =
            serde_json::from_str(r#"{"task":"x","limits":{"compress_threshold_chars":50000}}"#)
                .unwrap();
        let mut cfg = cfg_disabled;
        cfg.apply(partial).unwrap();
        assert_eq!(
            cfg.limits.compress_threshold_chars, 50000,
            "finite value adopted from a 0 (disabled) base"
        );

        // From a finite base, a task can only tighten (min), never loosen.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"compress_threshold_chars":100000}}"#,
        )
        .unwrap();
        let partial: PartialConfig =
            serde_json::from_str(r#"{"task":"x","limits":{"compress_threshold_chars":200000}}"#)
                .unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(
            cfg.limits.compress_threshold_chars, 100000,
            "larger threshold (compress later) must not loosen"
        );
    }

    #[test]
    fn stream_idle_secs_defaults_to_30_and_overridable() {
        // Deep-reasoning models with long first-token latency need the SSE idle
        // gap raised above the 30s default; it must be a real config field.
        let cfg: Config =
            serde_json::from_str(r#"{"provider":"anthropic","api_key_env":"K","model":"m"}"#)
                .unwrap();
        assert_eq!(cfg.limits.stream_idle_secs, 30, "default idle gap is 30s");

        let cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"stream_idle_secs":180}}"#,
        )
        .unwrap();
        assert_eq!(cfg.limits.stream_idle_secs, 180);
    }

    #[test]
    fn batch_overlay_stream_idle_only_tightens() {
        // stream_idle_secs: shorter = tighter (a stalled stream fails sooner).
        // A task may only shorten it, never lengthen it (the longer window
        // could mask a hang; the reqwest total timeout is the backstop).
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"stream_idle_secs":60}}"#,
        )
        .unwrap();
        let partial: PartialConfig =
            serde_json::from_str(r#"{"task":"x","limits":{"stream_idle_secs":15}}"#).unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(
            cfg.limits.stream_idle_secs, 15,
            "a shorter idle window tightens"
        );

        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"stream_idle_secs":60}}"#,
        )
        .unwrap();
        let partial: PartialConfig =
            serde_json::from_str(r#"{"task":"x","limits":{"stream_idle_secs":180}}"#).unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(
            cfg.limits.stream_idle_secs, 60,
            "a longer idle window must not loosen (mask a stall)"
        );
    }

    #[test]
    fn shell_timeout_defaults_to_120_and_overridable() {
        let cfg: Config =
            serde_json::from_str(r#"{"provider":"anthropic","api_key_env":"K","model":"m"}"#)
                .unwrap();
        assert_eq!(
            cfg.limits.shell_timeout_secs, 120,
            "default per-command timeout is 120s"
        );

        let cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"shell_timeout_secs":0}}"#,
        )
        .unwrap();
        assert_eq!(cfg.limits.shell_timeout_secs, 0, "0 = no per-command limit");
    }

    #[test]
    fn batch_overlay_shell_timeout_is_bidirectional() {
        // shell_timeout_secs is a cost knob, not a safety boundary: a task
        // fragment may RAISE it (eval builds legitimately run longer than the
        // 120s default) or lower it — the run-level timeout_secs and
        // max_total_tokens remain the real backstops. This is the ONLY limits
        // field applied without a clamp.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"shell_timeout_secs":120}}"#,
        )
        .unwrap();
        let partial: PartialConfig =
            serde_json::from_str(r#"{"task":"x","limits":{"shell_timeout_secs":600}}"#).unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(cfg.limits.shell_timeout_secs, 600, "raising is allowed");

        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"shell_timeout_secs":120}}"#,
        )
        .unwrap();
        let partial: PartialConfig =
            serde_json::from_str(r#"{"task":"x","limits":{"shell_timeout_secs":30}}"#).unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(cfg.limits.shell_timeout_secs, 30, "tightening is allowed");

        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"shell_timeout_secs":120}}"#,
        )
        .unwrap();
        let partial: PartialConfig =
            serde_json::from_str(r#"{"task":"x","limits":{"shell_timeout_secs":0}}"#).unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(
            cfg.limits.shell_timeout_secs, 0,
            "0 (no limit) is allowed too"
        );
    }

    #[test]
    fn clamp_merge_matrix() {
        // The three policies are the SPEC of what a batch fragment may do to a
        // limits field; every branch asserted, including the (0, 0) corner.
        // Tighten: plain min; None keeps the base.
        assert_eq!(Clamp::Tighten.merge(10u64, Some(100)), 10);
        assert_eq!(Clamp::Tighten.merge(10u64, Some(3)), 3);
        assert_eq!(Clamp::Tighten.merge(10u64, None), 10);
        // TightenOrInit: patch 0 ignored (loosening sentinel); a 0 (unlimited)
        // base adopts any finite value; two finite values take min; None keeps.
        assert_eq!(Clamp::TightenOrInit.merge(60u64, Some(0)), 60);
        assert_eq!(Clamp::TightenOrInit.merge(0u64, Some(30)), 30);
        assert_eq!(Clamp::TightenOrInit.merge(0u64, Some(0)), 0);
        assert_eq!(Clamp::TightenOrInit.merge(60u64, Some(30)), 30);
        assert_eq!(Clamp::TightenOrInit.merge(60u64, Some(120)), 60);
        assert_eq!(Clamp::TightenOrInit.merge(60u64, None), 60);
        // Replace: both directions allowed; None keeps the base.
        assert_eq!(Clamp::Replace.merge(120u64, Some(600)), 600);
        assert_eq!(Clamp::Replace.merge(120u64, Some(30)), 30);
        assert_eq!(Clamp::Replace.merge(120u64, Some(0)), 0);
        assert_eq!(Clamp::Replace.merge(120u64, None), 120);
    }

    #[test]
    fn limits_empty_json_equals_default() {
        // serde-default canary: `Limits` must keep parsing `{}` into exactly
        // `Limits::default()`. This is what killed the macro-based config
        // redesign (its generated struct lost the per-field serde defaults and
        // broke parse_minimal_config); pinned here against regression.
        let parsed: Limits = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, Limits::default());
    }

    #[test]
    fn apply_overlays_every_partial_limits_field() {
        // Anti-silence sentinel: EVERY PartialLimits field set to a
        // distinguishing (tightening) value must observably land after apply.
        // Catches a field added to the struct but never wired into apply() —
        // e.g. if someone reintroduces `..` in the exhaustive destructure.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m","limits":{
                "max_turns":100,"timeout_secs":1000,"max_output_chars":100000,
                "compress_threshold_chars":1000000,"keep_recent_messages":100,
                "max_context_tokens":1000000,"max_total_tokens":1000000,
                "stream_idle_secs":1000,"shell_timeout_secs":10000,
                "subagent_max_turns":1000}}"#,
        )
        .unwrap();
        let partial: PartialConfig = serde_json::from_str(
            r#"{"limits":{"max_turns":2,"timeout_secs":3,"max_output_chars":4,
                "compress_threshold_chars":5,"keep_recent_messages":6,
                "max_context_tokens":7,"max_total_tokens":8,
                "stream_idle_secs":9,"shell_timeout_secs":10,
                "subagent_max_turns":11}}"#,
        )
        .unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(cfg.limits.max_turns, 2, "Tighten field must land");
        assert_eq!(cfg.limits.timeout_secs, 3, "TightenOrInit field must land");
        assert_eq!(cfg.limits.max_output_chars, 4, "Tighten field must land");
        assert_eq!(
            cfg.limits.compress_threshold_chars, 5,
            "TightenOrInit field must land"
        );
        assert_eq!(
            cfg.limits.keep_recent_messages, 6,
            "Tighten field must land"
        );
        assert_eq!(
            cfg.limits.max_context_tokens, 7,
            "TightenOrInit field must land"
        );
        assert_eq!(
            cfg.limits.max_total_tokens, 8,
            "TightenOrInit field must land"
        );
        assert_eq!(cfg.limits.stream_idle_secs, 9, "Tighten field must land");
        assert_eq!(cfg.limits.shell_timeout_secs, 10, "Replace field must land");
        assert_eq!(
            cfg.limits.subagent_max_turns, 11,
            "TightenOrInit field must land"
        );
    }

    #[test]
    fn subagent_max_turns_fragment_can_only_tighten() {
        // The sub-agent turn ceiling is a safety boundary, so its Clamp is
        // TightenOrInit with 0 = "no separate ceiling" (the loosest value):
        // a fragment's 0 is a no-op, a finite fragment value tightens both a
        // finite and a 0 (unlimited) base, and the smaller always wins.
        let base = |turns: u32| {
            let json = format!(
                r#"{{"provider":"openai","api_key_env":"K","model":"m",
                     "limits":{{"subagent_max_turns":{turns}}}}}"#
            );
            serde_json::from_str::<Config>(&json).unwrap()
        };
        let fragment = |turns: u32| {
            let json = format!(r#"{{"limits":{{"subagent_max_turns":{turns}}}}}"#);
            serde_json::from_str::<PartialConfig>(&json).unwrap()
        };

        // 存量兼容：默认 25（原硬编码常量），未显式配置即拿到它。
        assert_eq!(Limits::default().subagent_max_turns, 25);
        assert_eq!(base(25).limits.subagent_max_turns, 25);

        let mut c = base(25);
        c.apply(fragment(5)).unwrap();
        assert_eq!(c.limits.subagent_max_turns, 5, "有限收紧必须生效");

        let mut c = base(25);
        c.apply(fragment(0)).unwrap();
        assert_eq!(
            c.limits.subagent_max_turns, 25,
            "片段的 0 是 no-op，不能关掉基座天花板"
        );

        let mut c = base(0);
        c.apply(fragment(5)).unwrap();
        assert_eq!(
            c.limits.subagent_max_turns, 5,
            "基座 0（不另设）可被有限值收紧"
        );

        let mut c = base(5);
        c.apply(fragment(25)).unwrap();
        assert_eq!(c.limits.subagent_max_turns, 5, "放宽被拒（只允许收紧）");
    }

    /// §9 per-field merge strategies for `code` fragments — the §13-18-style
    /// vector table: enabled is REJECTED (untrusted fragments cannot switch
    /// the sandbox), deny_paths unions+dedups, fs_write_dirs intersects with
    /// empty-as-universe semantics, and the numeric knobs clamp per their
    /// declared Clamp variant.
    #[test]
    fn code_fragment_merge_strategies() {
        let base: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m","inject_environment":false,
                "code":{"enabled":true,"deny_paths":[".env","secrets/**"],
                "fs_write_dirs":["src","docs"],"max_tool_calls":50,
                "timeout_secs":120,"fuel":1000000000}}"#,
        )
        .unwrap();

        // `enabled` present in ANY form → bad_task (even `true == base`).
        for enabled in ["true", "false"] {
            let frag: PartialConfig =
                serde_json::from_str(&format!(r#"{{"code":{{"enabled":{enabled}}}}}"#)).unwrap();
            let err = base.clone().apply(frag).unwrap_err().to_string();
            assert!(
                err.contains("code.enabled"),
                "enabled presence must be rejected: {err}"
            );
        }

        // Channel gates (include MD-13 / call_batch LD-15) are Tighten on
        // bools — false < true: a fragment may only turn them OFF, never ON,
        // and an absent key keeps the base.
        let gated_base: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
                "code":{"include":true,"call_batch":true}}"#,
        )
        .unwrap();
        let mut cfg = gated_base.clone();
        let frag: PartialConfig =
            serde_json::from_str(r#"{"code":{"include":false,"call_batch":false}}"#).unwrap();
        cfg.apply(frag).unwrap();
        assert!(!cfg.code.include, "fragment may turn include off");
        assert!(!cfg.code.call_batch, "fragment may turn call_batch off");
        let mut cfg = gated_base;
        let frag: PartialConfig =
            serde_json::from_str(r#"{"code":{"include":true,"call_batch":true}}"#).unwrap();
        cfg.apply(frag).unwrap();
        assert!(cfg.code.include, "fragment true == base true, no-op");
        assert!(cfg.code.call_batch, "fragment true == base true, no-op");
        // Default base (both off): a fragment asking to enable is silently
        // clamped back off — enabling channels is the trusted base's call.
        let mut cfg: Config =
            serde_json::from_str(r#"{"provider":"anthropic","api_key_env":"K","model":"m"}"#)
                .unwrap();
        let frag: PartialConfig =
            serde_json::from_str(r#"{"code":{"include":true,"call_batch":true}}"#).unwrap();
        cfg.apply(frag).unwrap();
        assert!(!cfg.code.include, "fragment may not enable include");
        assert!(!cfg.code.call_batch, "fragment may not enable call_batch");

        let frag: PartialConfig = serde_json::from_str(
            r#"{"code":{"deny_paths":[".env","logs/**"],"fs_write_dirs":["src","out"],
                "max_tool_calls":10,"mem_mb":64,"timeout_secs":30,
                "fuel":500000000,"max_result_chars":2000}}"#,
        )
        .unwrap();
        let mut cfg = base.clone();
        cfg.apply(frag).unwrap();
        let c = &cfg.code;
        // Union with dedup (.env already present).
        let mut deny = c.deny_paths.clone();
        deny.sort();
        assert_eq!(deny, [".env", "logs/**", "secrets/**"]);
        // Intersection: ["src","docs"] ∩ ["src","out"] = ["src"].
        assert_eq!(c.fs_write_dirs, ["src"]);
        // Tighten / TightenOrInit.
        assert_eq!(c.max_tool_calls, 10, "Tighten takes the smaller");
        assert_eq!(c.mem_mb, 64, "Tighten takes the smaller");
        assert_eq!(
            c.timeout_secs, 30,
            "TightenOrInit takes the smaller of two finite values"
        );
        assert_eq!(c.fuel, 500_000_000, "Tighten takes the smaller");
        assert_eq!(c.max_result_chars, 2000, "Tighten takes the smaller");

        // Tighten has NO 0-sentinel: a fragment 0 is the tightest value and
        // shuts the knob off entirely (fuel=0 / max_tool_calls=0 = the guest
        // cannot run / cannot call tools) — untrusted fragments may only
        // tighten, and "off" is the tightest.
        let frag: PartialConfig =
            serde_json::from_str(r#"{"code":{"fuel":0,"max_tool_calls":0}}"#).unwrap();
        let mut cfg = base.clone();
        cfg.apply(frag).unwrap();
        assert_eq!(cfg.code.fuel, 0, "Tighten: fragment 0 wins");
        assert_eq!(cfg.code.max_tool_calls, 0, "Tighten: fragment 0 wins");

        // fs_write_dirs universe semantics: fragment empty → base kept;
        // base empty → fragment adopted (default = whole workdir writable).
        let frag: PartialConfig = serde_json::from_str(r#"{"code":{"fs_write_dirs":[]}}"#).unwrap();
        let mut cfg = base.clone();
        cfg.apply(frag).unwrap();
        assert_eq!(
            cfg.code.fs_write_dirs,
            ["src", "docs"],
            "empty fragment = universe"
        );
        let mut cfg: Config =
            serde_json::from_str(r#"{"provider":"anthropic","api_key_env":"K","model":"m"}"#)
                .unwrap();
        let frag: PartialConfig =
            serde_json::from_str(r#"{"code":{"fs_write_dirs":["only"]}}"#).unwrap();
        cfg.apply(frag).unwrap();
        assert_eq!(cfg.code.fs_write_dirs, ["only"], "empty base = universe");

        // Disjoint intersection may legally empty out (tightened to
        // nowhere-writable — writes then all fail, reads still pass).
        let frag: PartialConfig =
            serde_json::from_str(r#"{"code":{"fs_write_dirs":["nowhere"]}}"#).unwrap();
        let mut cfg = base;
        cfg.apply(frag).unwrap();
        assert_eq!(cfg.code.fs_write_dirs, Vec::<String>::new());

        // timeout_secs TightenOrInit: fragment 0 (no limit) is the LOOSEST
        // value — ignored over a finite base.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
                "code":{"timeout_secs":60}}"#,
        )
        .unwrap();
        let frag: PartialConfig = serde_json::from_str(r#"{"code":{"timeout_secs":0}}"#).unwrap();
        cfg.apply(frag).unwrap();
        assert_eq!(
            cfg.code.timeout_secs, 60,
            "fragment 0 = no limit = loosest, ignored"
        );

        // u64::MAX is a finite value; TightenOrInit only adopts it over a 0
        // (no-limit) base — exactly how an untrusted batch fragment can land
        // it. The sandbox must then degrade it to "no wall clock" (checked
        // add, see run_code::huge_timeout_secs_degrades_to_no_wall_clock) —
        // the merge must at least accept it without panicking. Over the
        // finite default (120) the fragment is the LOOSER value and is
        // ignored, MAX never even reaches the sandbox.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
                "code":{"timeout_secs":0}}"#,
        )
        .unwrap();
        let frag: PartialConfig =
            serde_json::from_str(r#"{"code":{"timeout_secs":18446744073709551615}}"#).unwrap();
        cfg.apply(frag).unwrap();
        assert_eq!(cfg.code.timeout_secs, u64::MAX);
    }

    /// `code.llm` (code_llm_design §5, LD-5): parse-time enablement
    /// invariants + fragment strategies. Enabled-without-a-serving-default
    /// fails at CONFIG time (every entry point routes through
    /// validate_and_normalize); a fragment may only SHRINK `models`
    /// (intersection), and `enabled`/`default_model` presence is bad_task.
    #[test]
    fn llm_config_validation_and_fragment_strategies() {
        // Defaults: fully off, absent key round-trips.
        let cfg: Config =
            serde_json::from_str(r#"{"provider":"anthropic","api_key_env":"K","model":"m"}"#)
                .unwrap();
        assert!(!cfg.code.llm.enabled);
        assert_eq!(cfg.code.llm.default_model, None);
        assert!(cfg.code.llm.models.is_empty());
        let mut cfg = cfg;
        cfg.validate_and_normalize().unwrap();

        // Enablement invariants — each with its actionable message.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
                "code":{"llm":{"enabled":true}}}"#,
        )
        .unwrap();
        let err = cfg.validate_and_normalize().unwrap_err().to_string();
        assert!(err.contains("models"), "empty allowlist blamed: {err}");
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
                "code":{"llm":{"enabled":true,"models":["m1"],"default_model":null}}}"#,
        )
        .unwrap();
        let err = cfg.validate_and_normalize().unwrap_err().to_string();
        assert!(
            err.contains("default_model"),
            "missing default blamed: {err}"
        );
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
                "code":{"llm":{"enabled":true,"models":["m1"],"default_model":"other"}}}"#,
        )
        .unwrap();
        let err = cfg.validate_and_normalize().unwrap_err().to_string();
        assert!(
            err.contains("other") && err.contains("models"),
            "default-not-in-tier blamed: {err}"
        );
        // Valid shape passes; duplicate entries dedup stably (first
        // occurrence order).
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
                "code":{"llm":{"enabled":true,"models":["m1","m2","m1"],
                "default_model":"m1"}}}"#,
        )
        .unwrap();
        cfg.validate_and_normalize().unwrap();
        assert_eq!(cfg.code.llm.models, ["m1", "m2"]);
        // deny_unknown_fields inside the llm object.
        let e = serde_json::from_str::<Config>(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
                "code":{"llm":{"enabled":false,"modles":[]}}}"#,
        )
        .unwrap_err();
        assert!(e.to_string().contains("modles"));

        // Fragment presence of enabled / default_model → bad_task (even
        // value-equal-to-base).
        let base: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
                "code":{"llm":{"enabled":true,"models":["m1","m2"],
                "default_model":"m1"}}}"#,
        )
        .unwrap();
        for frag_json in [
            r#"{"code":{"llm":{"enabled":false}}}"#,
            r#"{"code":{"llm":{"enabled":true}}}"#,
            r#"{"code":{"llm":{"default_model":"m1"}}}"#,
        ] {
            let frag: PartialConfig = serde_json::from_str(frag_json).unwrap();
            let err = base.clone().apply(frag).unwrap_err().to_string();
            assert!(
                err.contains("code.llm."),
                "fragment must not set llm gates: {err} ({frag_json})"
            );
        }

        // models intersection: base order preserved; keeping the default is
        // fine; dropping it (even while keeping others) is bad_task.
        let frag: PartialConfig =
            serde_json::from_str(r#"{"code":{"llm":{"models":["m2","m1"]}}}"#).unwrap();
        let mut cfg = base.clone();
        cfg.apply(frag).unwrap();
        assert_eq!(cfg.code.llm.models, ["m1", "m2"], "base declaration order");
        let frag: PartialConfig =
            serde_json::from_str(r#"{"code":{"llm":{"models":["m2"]}}}"#).unwrap();
        let err = base.clone().apply(frag).unwrap_err().to_string();
        assert!(
            err.contains("m1") && err.contains("drops"),
            "dropped default blamed: {err}"
        );
        let frag: PartialConfig =
            serde_json::from_str(r#"{"code":{"llm":{"models":["m3"]}}}"#).unwrap();
        let err = base.clone().apply(frag).unwrap_err().to_string();
        assert!(err.contains("drops"), "disjoint overlay blamed: {err}");

        // A DISABLED base may narrow to anything, even empty — the tier is
        // inert until the trusted base enables it.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
                "code":{"llm":{"enabled":false,"models":["m1"],
                "default_model":"m1"}}}"#,
        )
        .unwrap();
        let frag: PartialConfig =
            serde_json::from_str(r#"{"code":{"llm":{"models":[]}}}"#).unwrap();
        cfg.apply(frag).unwrap();
        assert!(cfg.code.llm.models.is_empty());
    }

    /// A-2 (finetune_support): disclosure/intervention knobs merge as min
    /// under their orderings — a fragment can only narrow, never widen.
    /// `skill_injection`: Off < OnDemand < Eager (declaration order);
    /// `precheck`: false < true (a fragment may turn it OFF, never ON).
    #[test]
    fn code_fragment_skill_injection_and_precheck_clamp() {
        // Defaults: unconfigured base keeps status quo (NFR-1).
        let base: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m","inject_environment":false}"#,
        )
        .unwrap();
        assert_eq!(base.code.skill_injection, SkillInjection::OnDemand);
        assert!(!base.code.precheck);

        // Fragment eager over base off → off (the A-2 headline case).
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
                "code":{"skill_injection":"off","precheck":true}}"#,
        )
        .unwrap();
        let frag: PartialConfig =
            serde_json::from_str(r#"{"code":{"skill_injection":"eager","precheck":false}}"#)
                .unwrap();
        cfg.apply(frag).unwrap();
        assert_eq!(cfg.code.skill_injection, SkillInjection::Off);
        assert!(!cfg.code.precheck, "fragment may only turn precheck off");

        // Fragment wider than base is silently clamped to the base.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
                "code":{"skill_injection":"on_demand"}}"#,
        )
        .unwrap();
        let frag: PartialConfig =
            serde_json::from_str(r#"{"code":{"skill_injection":"eager"}}"#).unwrap();
        cfg.apply(frag).unwrap();
        assert_eq!(
            cfg.code.skill_injection,
            SkillInjection::OnDemand,
            "fragment may not widen disclosure"
        );

        // Tightening direction is legal: fragment off over base eager → off.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
                "code":{"skill_injection":"eager","precheck":true}}"#,
        )
        .unwrap();
        let frag: PartialConfig =
            serde_json::from_str(r#"{"code":{"skill_injection":"off","precheck":false}}"#).unwrap();
        cfg.apply(frag).unwrap();
        assert_eq!(cfg.code.skill_injection, SkillInjection::Off);
        assert!(!cfg.code.precheck);

        // Serde labels are the stable audit vocabulary (as_str consumers:
        // result.json / NDJSON).
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
                "code":{"skill_injection":"eager"}}"#,
        )
        .unwrap();
        assert_eq!(cfg.code.skill_injection.as_str(), "eager");
    }

    /// Secret-hygiene regression: `headers`/`env` carry credentials (Bearer
    /// tokens, GITHUB_PERSONAL_ACCESS_TOKEN, DB URLs — the `env` field's own
    /// doc), so NO Debug output — direct `McpServerConfig`, whole `Config`,
    /// or `PartialConfig` (both embed the type) — may contain the VALUES;
    /// the key NAMES and the "***" marker must survive (key names have
    /// diagnostic value, values are secrets — the same indirect design the
    /// API key itself uses via `api_key_env`).
    #[test]
    fn mcp_debug_masks_header_and_env_secret_values_and_keeps_key_names() {
        let secret_header = "sk-DEFINITELY-A-SECRET-BEARER-VALUE";
        let secret_env = "ghp_DEFINITELY-A-SECRET-TOKEN-VALUE";
        let json = format!(
            r#"{{"provider":"openai","api_key_env":"K","model":"m",
                "mcp_servers":[
                  {{"name":"gh","command":"npx","args":["-y","server-github"],
                    "env":{{"GITHUB_PERSONAL_ACCESS_TOKEN":"{secret_env}","CACHE":"1"}}}},
                  {{"name":"http","url":"http://h/mcp","transport":"http",
                    "headers":{{"Authorization":"Bearer {secret_header}","X-Trace":"t"}}}}
                ]}}"#
        );
        let cfg: Config = serde_json::from_str(&json).unwrap();

        // Direct Debug and the derived ones that embed the type (Config,
        // PartialConfig below): values never appear, the mask does.
        for printed in [
            format!("{:?}", cfg.mcp_servers[0]),
            format!("{:?}", cfg.mcp_servers[1]),
            format!("{cfg:?}"),
        ] {
            assert!(!printed.contains(secret_env), "env value leaked: {printed}");
            assert!(
                !printed.contains(secret_header),
                "header value leaked: {printed}"
            );
            assert!(
                !printed.contains("Bearer"),
                "scheme prefix leaked: {printed}"
            );
            assert!(
                printed.contains("\"***\""),
                "mask marker missing: {printed}"
            );
        }
        // Key names stay for BOTH maps, on the direct Debug and through
        // Config's derived one.
        assert!(format!("{:?}", cfg.mcp_servers[0]).contains("GITHUB_PERSONAL_ACCESS_TOKEN"));
        assert!(format!("{:?}", cfg.mcp_servers[1]).contains("Authorization"));
        assert!(format!("{cfg:?}").contains("X-Trace"));
        // Non-secret fields print normally — a fully opaque Debug would make
        // even the masking undiagnosable.
        assert!(format!("{:?}", cfg.mcp_servers[0]).contains("server-github"));

        // PartialConfig (the batch fragment shape) embeds the same type.
        let frag: PartialConfig = serde_json::from_str(&format!(
            r#"{{"mcp_servers":[{{"name":"x","url":"http://h",
                "headers":{{"Authorization":"Bearer {secret_header}"}}}}]}}"#
        ))
        .unwrap();
        let printed = format!("{frag:?}");
        assert!(
            !printed.contains(secret_header),
            "fragment leaked: {printed}"
        );
        assert!(printed.contains("\"***\""), "fragment masked: {printed}");
    }

    /// P2 follow-up to the test above: `McpServerConfig.url` may carry
    /// basic-auth userinfo (`https://user:pass@host`); the headers/env
    /// redaction round left the url field verbatim, so Debug still leaked
    /// credentials. The mask must hide username AND password while keeping
    /// the host/port/path (the diagnostic core), leave credential-free urls
    /// byte-identical, and survive malformed input — a Debug print must
    /// never become the thing that crashes on an untrusted config string.
    #[test]
    fn mcp_debug_masks_url_userinfo_credentials_and_keeps_host_and_plain_urls() {
        let dbg = |url: &str| {
            let s: McpServerConfig =
                serde_json::from_str(&format!(r#"{{"name":"x","url":"{url}"}}"#)).unwrap();
            format!("{s:?}")
        };

        // user:password — BOTH halves disappear, host/port/path/query stay.
        let p = dbg("https://alice:hunter2-secret@mc.example.com:8443/mcp?q=1");
        assert!(
            !p.contains("alice") && !p.contains("hunter2-secret"),
            "userinfo leaked: {p}"
        );
        assert!(
            p.contains("https://***@mc.example.com:8443/mcp?q=1"),
            "host/port/path must survive the mask: {p}"
        );

        // Username-only userinfo is still credentials (a login name leaks
        // even without a password).
        let p = dbg("https://bob@mc.example.com/mcp");
        assert!(!p.contains("bob"), "username leaked: {p}");
        assert!(p.contains("https://***@mc.example.com/mcp"), "{p}");

        // LAST '@' wins: splitting at the first would re-leak password
        // bytes after the mask (`user:p@ss@host`).
        let p = dbg("https://alice:p@ss@mc.example.com");
        assert!(!p.contains("alice") && !p.contains("p@ss"), "leaked: {p}");
        assert!(p.contains("https://***@mc.example.com"), "{p}");

        // No userinfo: byte-identical — masking clean urls would hide the
        // very thing a Debug print exists to show.
        let clean = "https://mc.example.com:8443/mcp";
        assert!(dbg(clean).contains(clean), "clean url must print verbatim");

        // '@' past the authority (path/query text) is NOT userinfo:
        // untouched, per the bounded scan.
        let at_in_path = "https://mc.example.com/p@th?q=ops@corp";
        assert!(dbg(at_in_path).contains(at_in_path), "{at_in_path}");

        // Empty userinfo (`@host`) carries nothing — verbatim, no bogus mask.
        let empty_user = "https://@mc.example.com";
        assert!(dbg(empty_user).contains(empty_user), "{empty_user}");

        // Malformed/untrusted garbage: must not panic (the format! call IS
        // the no-panic assertion — any slice misalignment panics here) and
        // prints as-is.
        for junk in ["", "::::", "://", "http://", "not-a-url", "http://@"] {
            let p = dbg(junk);
            assert!(
                p.contains(junk),
                "malformed input must print verbatim: {junk:?} -> {p}"
            );
        }

        // The mask rides the whole containment chain: Config's derived Debug
        // routes through this impl, same as the headers/env redaction.
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m",
                "mcp_servers":[{"name":"x",
                "url":"https://alice:hunter2-secret@mc.example.com"}]}"#,
        )
        .unwrap();
        let p = format!("{cfg:?}");
        assert!(
            !p.contains("alice") && !p.contains("hunter2-secret"),
            "Config Debug leaked url credentials: {p}"
        );
        assert!(p.contains("***@"), "mask marker missing: {p}");
    }

    /// Library-API defense regression: after the bin/lib split `apply` is a
    /// published API, so the hard-field screen cannot rely on the caller
    /// running forbidden_batch_override first (the historical CLI-only
    /// discipline). EVERY hard field, set alone, is rejected IN-METHOD with
    /// the field named and the escape hatches pointed at; soft fields keep
    /// flowing through apply() itself.
    #[test]
    fn apply_rejects_every_hard_field_in_method_and_points_at_apply_trusted() {
        let base: Config =
            serde_json::from_str(r#"{"provider":"anthropic","api_key_env":"K","model":"m"}"#)
                .unwrap();
        for (json, field) in [
            (r#"{"provider":"openai"}"#, "provider"),
            (r#"{"base_url":"http://evil"}"#, "base_url"),
            (r#"{"api_key_env":"STOLEN_KEY_VAR"}"#, "api_key_env"),
            (
                r#"{"workdir":"/nonexistent-the-guard-fires-before-fs-access"}"#,
                "workdir",
            ),
            (r#"{"tools":["shell"]}"#, "tools"),
            (r#"{"restrict_to_workdir":false}"#, "restrict_to_workdir"),
            (
                r#"{"mcp_servers":[{"name":"x","command":"npx"}]}"#,
                "mcp_servers",
            ),
        ] {
            let mut cfg = base.clone();
            let p: PartialConfig = serde_json::from_str(json).unwrap();
            let err = cfg.apply(p).unwrap_err().to_string();
            assert!(err.contains(field), "field named: {err} ({json})");
            assert!(err.contains("apply_trusted"), "escape hatch named: {err}");
            // The screen fires before any field lands (the workdir case also
            // proves the guard wins over resolve_workdir's fs error).
            assert_eq!(cfg.provider, ProviderKind::Anthropic, "base untouched");
            assert!(cfg.mcp_servers.is_empty(), "no server injected");
        }
        // Soft fields still flow through apply() itself.
        let mut cfg = base;
        let p: PartialConfig =
            serde_json::from_str(r#"{"model":"task-model","task":"t"}"#).unwrap();
        cfg.apply(p).unwrap();
        assert_eq!(cfg.model, "task-model");
        assert_eq!(cfg.task.as_deref(), Some("t"));
    }

    /// apply_trusted is the host-vetted overlay path: hard fields land, and
    /// the two validations the old unchecked path was missing fire THERE —
    /// mcp_servers validate()d eagerly (same promise as Config::load) and
    /// tools names checked under the agent-construction rules.
    #[test]
    fn apply_trusted_applies_hard_fields_and_validates_mcp_servers_and_tool_names() {
        let mut cfg: Config =
            serde_json::from_str(r#"{"provider":"anthropic","api_key_env":"K","model":"m"}"#)
                .unwrap();
        let p: PartialConfig = serde_json::from_str(
            r#"{"provider":"openai","base_url":"http://localhost:8000",
                "api_key_env":"OTHER_KEY_VAR","tools":["shell","read_file"],
                "mcp_servers":[{"name":"fs","command":"npx"}]}"#,
        )
        .unwrap();
        cfg.apply_trusted(p).unwrap();
        assert_eq!(cfg.provider, ProviderKind::Openai, "hard provider applied");
        assert_eq!(cfg.base_url.as_deref(), Some("http://localhost:8000"));
        assert_eq!(cfg.api_key_env, "OTHER_KEY_VAR");
        assert_eq!(cfg.tools, ["shell", "read_file"]);
        assert_eq!(cfg.mcp_servers.len(), 1, "valid mcp server applied");

        // Bad MCP server (command AND url — the exact shape validate()
        // rejects) fails at overlay time, blamed on the named server.
        let p: PartialConfig = serde_json::from_str(
            r#"{"mcp_servers":[{"name":"broken","command":"npx","url":"http://h"}]}"#,
        )
        .unwrap();
        let err = cfg.apply_trusted(p).unwrap_err().to_string();
        assert!(err.contains("broken"), "bad server blamed: {err}");

        // Tool names: the agent-construction rules, string edition — bad
        // charset, duplicates and empty each rejected with the cause blamed.
        for (tools_json, must) in [
            (r#"["shell","bad name!"]"#, "bad name!"),
            (r#"["shell","shell"]"#, "duplicate"),
            (r#"[""]"#, "must not be empty"),
        ] {
            let p: PartialConfig =
                serde_json::from_str(&format!(r#"{{"tools":{tools_json}}}"#)).unwrap();
            let err = cfg.apply_trusted(p).unwrap_err().to_string();
            assert!(err.contains(must), "tool-name rule fired ({must}): {err}");
        }
    }
}
