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
    100_000
}
fn default_keep_recent() -> usize {
    8
}
fn default_max_tokens() -> u32 {
    8192
}
fn default_tools() -> Vec<String> {
    vec![
        "shell".into(),
        "read_file".into(),
        "write_file".into(),
        "edit_file".into(),
        "list_dir".into(),
    ]
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_output")]
    pub max_output_chars: usize,
    /// Compress conversation history beyond this many chars (0 = never).
    #[serde(default = "default_compress_threshold")]
    pub compress_threshold_chars: usize,
    /// Messages kept verbatim at the tail when compressing.
    #[serde(default = "default_keep_recent")]
    pub keep_recent_messages: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_turns: default_max_turns(),
            timeout_secs: default_timeout(),
            max_output_chars: default_max_output(),
            compress_threshold_chars: default_compress_threshold(),
            keep_recent_messages: default_keep_recent(),
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
    #[serde(default)]
    pub limits: Limits,
    /// External MCP tool servers (stdio transport).
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config file: {}", path.display()))?;
        let mut cfg: Config = serde_json::from_str(&raw)
            .with_context(|| format!("invalid config JSON: {}", path.display()))?;
        for s in &cfg.mcp_servers {
            s.validate()?;
        }
        if let Some(dir) = &cfg.workdir {
            let dir = dir
                .canonicalize()
                .with_context(|| format!("workdir does not exist: {}", dir.display()))?;
            if !dir.is_dir() {
                bail!("workdir is not a directory: {}", dir.display());
            }
            cfg.workdir = Some(dir);
        }
        Ok(cfg)
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
    pub fn apply(&mut self, p: PartialConfig) -> Result<()> {
        if let Some(v) = p.provider {
            self.provider = v;
        }
        if p.base_url.is_some() {
            self.base_url = p.base_url;
        }
        if let Some(v) = p.api_key_env {
            self.api_key_env = v;
        }
        if let Some(v) = p.model {
            self.model = v;
        }
        if let Some(v) = p.max_tokens {
            self.max_tokens = v;
        }
        if p.system_prompt.is_some() {
            self.system_prompt = p.system_prompt;
        }
        if p.task.is_some() {
            self.task = p.task;
        }
        if let Some(dir) = p.workdir {
            let dir = dir
                .canonicalize()
                .with_context(|| format!("workdir does not exist: {}", dir.display()))?;
            if !dir.is_dir() {
                bail!("workdir is not a directory: {}", dir.display());
            }
            self.workdir = Some(dir);
        }
        if let Some(v) = p.tools {
            self.tools = v;
        }
        if let Some(v) = p.restrict_to_workdir {
            self.restrict_to_workdir = v;
        }
        if let Some(l) = p.limits {
            // Batch task files are untrusted: a task may only TIGHTEN resource
            // limits (shorter timeout / fewer turns / smaller output), never
            // loosen them — otherwise it could set timeout_secs=0 (no limit)
            // and max_turns=u32::MAX and pin a worker forever. 0 means "no
            // limit" so it is treated as a loosening and ignored.
            if let Some(v) = l.max_turns {
                self.limits.max_turns = v.min(self.limits.max_turns);
            }
            if let Some(v) = l.timeout_secs {
                if v != 0 {
                    // timeout_secs==0 uniquely means "no limit" (the LOOSEST
                    // value), so v.min(0)==0 would silently drop a task's
                    // tightening to e.g. 30s — the opposite of every other
                    // field where 0 is the tightest. When the base is 0, take v
                    // directly (any finite v tightens); otherwise min so a task
                    // can only tighten, never loosen.
                    self.limits.timeout_secs = if self.limits.timeout_secs == 0 {
                        v
                    } else {
                        v.min(self.limits.timeout_secs)
                    };
                }
            }
            if let Some(v) = l.max_output_chars {
                self.limits.max_output_chars = v.min(self.limits.max_output_chars);
            }
            if let Some(v) = l.compress_threshold_chars {
                self.limits.compress_threshold_chars = v.min(self.limits.compress_threshold_chars);
            }
            if let Some(v) = l.keep_recent_messages {
                self.limits.keep_recent_messages = v.min(self.limits.keep_recent_messages);
            }
        }
        if let Some(v) = p.mcp_servers {
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
}

impl PartialConfig {
    /// First security-relevant field this fragment tries to set, if any.
    /// Batch task files are untrusted: they may only override soft fields
    /// (task / system_prompt / model / max_tokens / limits). Everything else
    /// (endpoints, credentials, tool whitelist, sandbox, MCP servers) must
    /// come from the trusted base config and must not be weakened per task.
    pub fn forbidden_batch_override(&self) -> Option<&'static str> {
        if self.provider.is_some() {
            return Some("provider");
        }
        if self.base_url.is_some() {
            return Some("base_url");
        }
        if self.api_key_env.is_some() {
            return Some("api_key_env");
        }
        if self.workdir.is_some() {
            return Some("workdir");
        }
        if self.tools.is_some() {
            return Some("tools");
        }
        if self.restrict_to_workdir.is_some() {
            return Some("restrict_to_workdir");
        }
        if self.mcp_servers.is_some() {
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(cfg.tools, vec!["shell", "read_file", "write_file", "edit_file", "list_dir"]);
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
}
