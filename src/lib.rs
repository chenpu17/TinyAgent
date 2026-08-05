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
pub mod mcp;
pub mod provider;
pub mod retry;
pub mod session;
pub mod tools;

// ── Facade re-exports (the library's public face) ──────────────────────────
pub use agent::{Agent, AgentEvent, CompressTrigger, RunCost, RunError, RunOutcome, ToolCallRecord};
pub use config::{Config, Limits};
pub use provider::{Block, ChatRequest, ChatResponse, Message, Provider, Role, ToolDef, Usage};
pub use tools::{CallbackTool, Tool, ToolCtx, ToolOutput};

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
/// an [`Agent`] directly via [`Agent::new`].
pub fn build_agent(
    cfg: &Config,
    initial: Vec<Message>,
    client: &reqwest::blocking::Client,
) -> anyhow::Result<Agent> {
    use anyhow::Context;
    let workdir = cfg
        .workdir
        .clone()
        .unwrap_or(std::env::current_dir().context("cannot determine current directory")?);
    let api_key = cfg.api_key()?;

    // Build MCP tools ONCE. The parent boxes them into its own tool list, and
    // tools::build hands SubAgent a clone of the same set — so a `task` call
    // shares the parent's MCP connections instead of re-spawning every server.
    let mcp_tools = if !cfg.mcp_servers.is_empty() {
        tools::mcp::build(&cfg.mcp_servers)?
    } else {
        Vec::new()
    };
    let mut tool_list = tools::build(&cfg.tools, cfg, client, &api_key, &mcp_tools)?;
    tool_list.extend(mcp_tools.into_iter().map(|t| Box::new(t) as Box<dyn tools::Tool>));

    let ctx = ToolCtx {
        workdir,
        max_output_chars: cfg.limits.max_output_chars,
        restrict_to_workdir: cfg.restrict_to_workdir,
        cancel: None,
    };
    Ok(Agent::new(cfg, provider::build(cfg, &api_key, client.clone()), tool_list, initial, ctx))
}
