//! TinyAgent — a minimal, single-binary AI agent exposed as a library.
//!
//! The crate is both a binary (`tiny-agent`, unchanged CLI / TUI) and this
//! library (`tiny_agent`), so third-party Rust code — and the Python / TypeScript
//! bindings built on top of it — can drive the agent loop programmatically:
//! construct an [`Agent`], stream [`AgentEvent`]s, register custom tools, and
//! read back honest token usage.
//!
//! The binary target depends on this library; `src/main.rs` only adds the
//! process-level pieces that don't belong in a library (CLI argument parsing,
//! SIGINT handling, `process::exit`, and the optional TUI).
//!
//! Quick start:
//! ```no_run
//! use tiny_agent::{build_agent, AgentEvent, Config};
//! let cfg = Config::load(std::path::Path::new("chat.json"))?;
//! let client = tiny_agent::provider::new_client(cfg.limits.timeout_secs);
//! let mut agent = build_agent(&cfg, Vec::new(), &client)?;
//! agent.run(&mut |ev: AgentEvent| match ev {
//!     tiny_agent::AgentEvent::TextDelta(t) => print!("{t}"),
//!     _ => {}
//! })?;
//! # Ok::<(), anyhow::Error>(())
//! ```

pub mod agent;
pub mod batch;
pub mod compress;
pub mod config;
pub mod harness;
pub mod hooks;
pub mod mcp;
pub mod provider;
pub mod retry;
pub mod session;
pub mod skills;
pub mod tools;

// ── Facade re-exports (the library's public face) ──────────────────────────
pub use agent::{
    extract_plan, Agent, AgentEvent, CompressOutcome, CompressTrigger, RunCost, RunError,
    RunOutcome, TodoItem, ToolCallRecord,
};
pub use config::{Config, Limits};
pub use hooks::{Hooks, ToolDecision};
pub use provider::{
    Block, ChatRequest, ChatResponse, Message, Provider, Role, SimpleChatResponse, ToolDef, Usage,
};
pub use tools::{CallbackTool, Tool, ToolConcurrency, ToolCtx, ToolOutput};

/// Wire together provider + tools (the config's built-in whitelist + any MCP
/// servers) + [`Agent`], returning a ready-to-run agent.
///
/// This is the same assembly the `tiny-agent` binary uses, factored out so
/// library users (and the Python / TypeScript bindings) don't have to know the
/// provider/tools/MCP wiring. `cfg.api_key()` reads the key from the configured
/// env var and `workdir` falls back to the current directory — mirroring the
/// binary — so behavior is identical.
///
/// For full control (custom [`Provider`] or a hand-built tool list), construct
/// an [`Agent`] directly via [`Agent::try_new`] (tool names are validated —
/// duplicates / illegal names are an error, not a silent shadow), or use
/// [`build_agent_with`] to add extra tools on top of the config's whitelist.
pub fn build_agent(
    cfg: &Config,
    initial: Vec<Message>,
    client: &reqwest::blocking::Client,
) -> anyhow::Result<Agent> {
    let api_key = cfg.api_key()?;
    build_agent_with(cfg, initial, client, Vec::new(), &api_key)
}

/// Like [`build_agent`], but appends `extra_tools` (e.g. a Python/TypeScript
/// caller's custom callbacks wrapped in [`CallbackTool`]) after the config's
/// built-in + MCP tools, and takes the API key explicitly (so a library caller
/// can pass one without it being read from the environment). The agent
/// dispatches by tool name, so an extra tool effectively extends the model's
/// available toolset.
pub fn build_agent_with(
    cfg: &Config,
    initial: Vec<Message>,
    client: &reqwest::blocking::Client,
    extra_tools: Vec<Box<dyn tools::Tool>>,
    api_key: &str,
) -> anyhow::Result<Agent> {
    let (tool_list, ctx, cfg) = assemble(cfg, client, extra_tools, api_key)?;
    Agent::try_new(
        &cfg,
        provider::build(&cfg, api_key, client.clone()),
        tool_list,
        initial,
        ctx,
    )
}

/// Like [`build_agent_with`], but the caller supplies the [`Provider`] directly
/// instead of it being built from the config. The Python / TypeScript bindings
/// use this to inject a user-supplied custom provider (host-language function
/// wrapped in a `Provider` impl).
///
/// `api_key` is still required because the built-in tool whitelist may include
/// `task` (sub-agent), whose child agents build their *own* provider from the
/// config + key. A purely custom-provider setup with no `task` tool can pass an
/// empty string. The injected `provider` drives the **main** agent only.
pub fn build_agent_with_provider(
    cfg: &Config,
    provider: Box<dyn Provider>,
    initial: Vec<Message>,
    client: &reqwest::blocking::Client,
    extra_tools: Vec<Box<dyn tools::Tool>>,
    api_key: &str,
) -> anyhow::Result<Agent> {
    let (tool_list, ctx, cfg) = assemble(cfg, client, extra_tools, api_key)?;
    Agent::try_new(&cfg, provider, tool_list, initial, ctx)
}

/// Shared tool-list + [`ToolCtx`] assembly for the `build_agent*` helpers:
/// resolve `workdir`, build the config's MCP tools once, wire the config's
/// built-in tool whitelist (passing the shared MCP set so a `task` sub-agent
/// reuses the parent's connections rather than re-spawning), then append MCP +
/// any extra tools. Factored out so [`build_agent_with`] and
/// [`build_agent_with_provider`] stay in lockstep — only the provider
/// construction differs between them.
#[allow(clippy::type_complexity)] // returns (tools, ctx, cfg): cfg carries the skill-augmented system_prompt
fn assemble(
    cfg: &Config,
    client: &reqwest::blocking::Client,
    extra_tools: Vec<Box<dyn tools::Tool>>,
    api_key: &str,
) -> anyhow::Result<(Vec<Box<dyn tools::Tool>>, ToolCtx, Config)> {
    use anyhow::Context;
    let workdir = cfg
        .workdir
        .clone()
        .unwrap_or(std::env::current_dir().context("cannot determine current directory")?);

    // Build MCP tools ONCE. The parent boxes them into its own tool list, and
    // tools::build hands SubAgent a clone of the same set — so a `task` call
    // shares the parent's MCP connections instead of re-spawning every server.
    let mcp_tools = if !cfg.mcp_servers.is_empty() {
        tools::mcp::build(&cfg.mcp_servers)?
    } else {
        Vec::new()
    };
    let mut tool_list = tools::build(&cfg.tools, cfg, client, api_key, &mcp_tools)?;
    tool_list.extend(
        mcp_tools
            .into_iter()
            .map(|t| Box::new(t) as Box<dyn tools::Tool>),
    );
    tool_list.extend(extra_tools);

    // Skills (progressive disclosure, agentskills.io open standard): scan global
    // + project dirs, preload only each skill's name+description into the system
    // prompt (level 1), and pair ReadSkill (load SKILL.md / a resource file →
    // history, levels 2/3) + ListSkill (discover a skill's bundled files, level
    // 3). One bad skill never breaks startup — malformed entries are skipped
    // with a stderr warning. Neither tool is in the config whitelist; both are
    // injected here, so sub-agents (which build their own whitelist-based tool
    // list) don't carry them. The intro is appended to a clone of the config
    // (assembly never mutates caller state); Agent::new then reads the augmented
    // system_prompt from this clone.
    let skill_dirs = resolve_skill_dirs(&workdir);
    let (index, warnings) = skills::scan(&skill_dirs);
    for w in &warnings {
        eprintln!("[skill] {w}");
    }
    // allowed-tools (experimental frontmatter field): warn-only — TinyAgent has
    // no per-turn permission/activation layer, so it can't grant a skill's
    // declared tools for a single turn the way Claude Code does. Surface a
    // declared tool that isn't available so the author notices the mismatch;
    // literal-name match only (the spec is space-separated tool names; Claude
    // Code's glob/`Tool(spec)` patterns are an extension we don't support).
    for e in index.entries() {
        for t in &e.allowed_tools {
            let known = cfg.tools.iter().any(|c| c == t) || t == "read_skill" || t == "list_skill";
            if !known {
                eprintln!(
                    "[skill] {} declares allowed-tool {:?} not in the tool whitelist",
                    e.name, t
                );
            }
        }
    }
    let mut cfg = cfg.clone();
    if !index.is_empty() {
        cfg.system_prompt = Some(skills::render_intro(cfg.system_prompt.as_deref(), &index));
        tool_list.push(Box::new(tools::ReadSkill::new(index.clone())) as Box<dyn tools::Tool>);
        tool_list.push(Box::new(tools::ListSkill::new(index)) as Box<dyn tools::Tool>);
    }

    // Startup contract: the FINAL list's names (built-in + MCP + skill + extra)
    // must be valid + unique. Without this a custom tool named like a built-in
    // was silently SHADOWED (dispatch resolves by first match) while the model
    // saw duplicate definitions — providers reject those with a 400 mid-run —
    // and an invalid charset/length only failed inside a provider request.
    // Agent::try_new runs the same check, so direct construction is covered
    // too; this call keeps the failure attributed to assembly.
    tools::validate_tool_names(&tool_list)?;

    let ctx = ToolCtx {
        workdir,
        max_output_chars: cfg.limits.max_output_chars,
        restrict_to_workdir: cfg.restrict_to_workdir,
        cancel: None,
        shell_timeout_secs: cfg.limits.shell_timeout_secs,
        hooks: None,
        subagent: None,
        budget: None,
        deadline: None,
    };
    Ok((tool_list, ctx, cfg))
}

/// Skill search dirs, global first + project last (so the project dir overrides
/// the global on a name clash). Missing dirs are fine — `scan` silently skips.
fn resolve_skill_dirs(workdir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(g) = skills::default_global_skills_dir() {
        dirs.push(g);
    }
    dirs.push(workdir.join(".tinyagent").join("skills"));
    dirs
}
