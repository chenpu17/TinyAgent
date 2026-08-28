use anyhow::{bail, Context, Result};
use serde::Deserialize;
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
fn default_max_tokens() -> u32 {
    8192
}
fn default_inject_environment() -> bool {
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

#[derive(Debug, Clone, Deserialize)]
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

impl McpServerConfig {
    pub fn validate(&self) -> Result<()> {
        match (&self.command, &self.url) {
            (Some(_), Some(_)) => {
                bail!("mcp server '{}': set either \"command\" or \"url\", not both", self.name)
            }
            (None, None) => {
                bail!("mcp server '{}': one of \"command\" or \"url\" is required", self.name)
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
        Ok(())
    }

    pub fn base_url(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| self.provider.default_base_url().to_string())
    }

    pub fn api_key(&self) -> Result<String> {
        std::env::var(&self.api_key_env)
            .with_context(|| format!("environment variable {} is not set", self.api_key_env))
    }

    /// Overlay a partial task config (batch mode): any `Some` field wins.
    ///
    /// The `PartialConfig` is destructured WITHOUT `..` at the top: adding a
    /// field to it without classifying it here (soft overlay) AND in
    /// `forbidden_batch_override` (hard/soft) is a compile error — closing
    /// the historical "field added, wiring forgotten, override silently
    /// dropped" hole (and its security-flavored sibling: a hard field
    /// forgotten from the forbidden list) at the type level, not by
    /// discipline. Never reintroduce `..` in these destructures.
    pub fn apply(&mut self, p: PartialConfig) -> Result<()> {
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
        }
        if let Some(v) = mcp_servers {
            self.mcp_servers = v;
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
}

impl PartialConfig {
    /// First security-relevant field this fragment tries to set, if any.
    /// Batch task files are untrusted: they may only override soft fields
    /// (task / system_prompt / model / max_tokens / reasoning_effort /
    /// inject_environment / limits). Everything else (endpoints, credentials,
    /// working directory, tool whitelist, sandbox, MCP servers) must come
    /// from the trusted base config and must not be weakened per task.
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
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m"}"#,
        )
        .unwrap();
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
        assert_eq!(cfg.tools, vec!["shell", "read_file", "write_file", "edit_file", "list_dir", "task", "todo"]);
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
        assert_eq!(cfg.mcp_servers[0].env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(cfg.mcp_servers[0].env.get("BAZ").map(String::as_str), Some("qux"));
    }

    #[test]
    fn partial_config_overlays_reasoning_effort_without_being_forbidden() {
        // reasoning_effort is a soft field like model/max_tokens: a batch task
        // fragment may override it, and it is NOT in forbidden_batch_override.
        let mut cfg: Config =
            serde_json::from_str(r#"{"provider":"anthropic","api_key_env":"K","model":"m"}"#)
                .unwrap();
        let partial: PartialConfig =
            serde_json::from_str(r#"{"reasoning_effort":"low"}"#).unwrap();
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
        let both: McpServerConfig = serde_json::from_str(
            r#"{"name":"x","command":"npx","url":"http://h/mcp"}"#,
        )
        .unwrap();
        assert!(both.validate().is_err());

        let neither: McpServerConfig = serde_json::from_str(r#"{"name":"x"}"#).unwrap();
        assert!(neither.validate().is_err());

        let stdio_with_headers: McpServerConfig = serde_json::from_str(
            r#"{"name":"x","command":"npx","headers":{"A":"b"}}"#,
        )
        .unwrap();
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
        assert_eq!(cfg.limits.timeout_secs, 30, "0-base must adopt the finite task value");

        // From a finite base the task can still only tighten (min), never loosen.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m",
               "limits":{"timeout_secs":60}}"#,
        )
        .unwrap();
        let partial: PartialConfig =
            serde_json::from_str(r#"{"task":"x","limits":{"timeout_secs":120}}"#).unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(cfg.limits.timeout_secs, 60, "a larger task timeout must not loosen the base");
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
        assert_eq!(cfg.limits.max_context_tokens, 200000, "0 sentinel must not disable");

        // From a disabled base (0), a finite task value is adopted.
        let mut cfg: Config = serde_json::from_str(
            r#"{"provider":"anthropic","api_key_env":"K","model":"m"}"#,
        )
        .unwrap();
        assert_eq!(cfg.limits.max_context_tokens, 0);
        let partial: PartialConfig =
            serde_json::from_str(r#"{"task":"x","limits":{"max_context_tokens":8000}}"#).unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(cfg.limits.max_context_tokens, 8000, "finite value adopted from 0 base");

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
        let partial: PartialConfig = serde_json::from_str(
            r#"{"task":"x","limits":{"max_context_tokens":200000}}"#,
        )
        .unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(cfg.limits.max_context_tokens, 100000, "larger task value must not loosen");
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
            serde_json::from_str(r#"{"task":"x","limits":{"compress_threshold_chars":0}}"#).unwrap();
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
            serde_json::from_str(r#"{"task":"x","limits":{"compress_threshold_chars":50000}}"#).unwrap();
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
            serde_json::from_str(r#"{"task":"x","limits":{"compress_threshold_chars":200000}}"#).unwrap();
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
            serde_json::from_str(r#"{"provider":"anthropic","api_key_env":"K","model":"m"}"#).unwrap();
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
        assert_eq!(cfg.limits.stream_idle_secs, 15, "a shorter idle window tightens");

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
            serde_json::from_str(r#"{"provider":"anthropic","api_key_env":"K","model":"m"}"#).unwrap();
        assert_eq!(cfg.limits.shell_timeout_secs, 120, "default per-command timeout is 120s");

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
        assert_eq!(cfg.limits.shell_timeout_secs, 0, "0 (no limit) is allowed too");
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
                "stream_idle_secs":1000,"shell_timeout_secs":10000}}"#,
        )
        .unwrap();
        let partial: PartialConfig = serde_json::from_str(
            r#"{"limits":{"max_turns":2,"timeout_secs":3,"max_output_chars":4,
                "compress_threshold_chars":5,"keep_recent_messages":6,
                "max_context_tokens":7,"max_total_tokens":8,
                "stream_idle_secs":9,"shell_timeout_secs":10}}"#,
        )
        .unwrap();
        cfg.apply(partial).unwrap();
        assert_eq!(cfg.limits.max_turns, 2, "Tighten field must land");
        assert_eq!(cfg.limits.timeout_secs, 3, "TightenOrInit field must land");
        assert_eq!(cfg.limits.max_output_chars, 4, "Tighten field must land");
        assert_eq!(cfg.limits.compress_threshold_chars, 5, "TightenOrInit field must land");
        assert_eq!(cfg.limits.keep_recent_messages, 6, "Tighten field must land");
        assert_eq!(cfg.limits.max_context_tokens, 7, "TightenOrInit field must land");
        assert_eq!(cfg.limits.max_total_tokens, 8, "TightenOrInit field must land");
        assert_eq!(cfg.limits.stream_idle_secs, 9, "Tighten field must land");
        assert_eq!(cfg.limits.shell_timeout_secs, 10, "Replace field must land");
    }
}
