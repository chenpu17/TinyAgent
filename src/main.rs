mod agent;
mod batch;
mod config;
mod harness;
mod mcp;
mod provider;
mod retry;
mod session;
mod signal;
mod tools;
#[cfg(feature = "tui")]
mod tui;

use agent::Agent;
use anyhow::{bail, Context, Result};
use clap::Parser;
use config::Config;
use provider::{Block, Message, Role};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tools::ToolCtx;

#[derive(Parser)]
#[command(
    name = "tiny-agent",
    about = "A minimal single-binary AI agent: one JSON config, two modes"
)]
struct Cli {
    /// Path to the JSON config file
    #[arg(long)]
    config: PathBuf,

    /// Interactive TUI chat mode (requires the `tui` build feature)
    #[arg(long)]
    tui: bool,

    /// Override the task from the config (headless mode)
    #[arg(long)]
    task: Option<String>,

    /// Override the model from the config
    #[arg(long)]
    model: Option<String>,

    /// Override limits.max_turns
    #[arg(long)]
    max_turns: Option<u32>,

    /// Override limits.timeout_secs (0 = no timeout)
    #[arg(long)]
    timeout: Option<u64>,

    /// Headless: emit one JSON object per agent event on stdout (NDJSON) instead
    /// of the human-readable format. Good for piping into another program.
    #[arg(long)]
    json: bool,

    /// Write a structured result JSON here when done (headless mode)
    #[arg(long)]
    output: Option<PathBuf>,

    /// Attach an image to the initial message (repeatable)
    #[arg(long)]
    image: Vec<PathBuf>,

    /// Session file for conversation persistence. Pass `none` to disable
    /// persistence entirely (no read on --resume, no per-turn write) — handy
    /// for one-shot headless runs that shouldn't touch the cwd.
    #[arg(long, default_value = ".tinyagent-session.json")]
    session: PathBuf,

    /// Resume from the session file instead of starting fresh
    #[arg(long)]
    resume: bool,

    /// Batch mode: run every *.json task fragment in this directory
    #[arg(long)]
    batch: Option<PathBuf>,

    /// Batch mode: number of parallel workers
    #[arg(long, default_value = "1")]
    jobs: usize,

    /// Batch mode: summary report output path
    #[arg(long, default_value = "report.json")]
    report: PathBuf,

    /// Retry on transient failures (provider/network errors) with exponential
    /// backoff. In single-run headless mode the agent run is retried in place;
    /// in `--batch` mode each task is retried. Deterministic failures (bad task
    /// file, max_turns, end_turn) are not retried.
    #[arg(long, default_value = "0")]
    retries: usize,

    /// Batch mode: re-run only tasks that failed in the prior report (at --report)
    #[arg(long)]
    failed: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cli_task = cli.task.clone();
    let mut cfg = Config::load(&cli.config)?;
    if let Some(t) = cli.task {
        cfg.task = Some(t);
    }
    // CLI overrides for quick A/B runs without editing the config.
    if let Some(m) = cli.model {
        cfg.model = m;
    }
    if let Some(n) = cli.max_turns {
        cfg.limits.max_turns = n;
    }
    if let Some(s) = cli.timeout {
        cfg.limits.timeout_secs = s;
    }

    // Resolve the session target up front: `--session none` is the opt-out
    // sentinel (no read on --resume, no per-turn write), anything else is a
    // file path. Computed once here so every mode below treats it the same.
    let session = session_target(&cli.session);

    // ── Batch mode ────────────────────────────────────────────────────
    if let Some(dir) = &cli.batch {
        if cli.tui {
            bail!("--batch and --tui are mutually exclusive");
        }
        // Wire Ctrl-C into batch the same way single-run headless does: first
        // SIGINT flips a shared flag the worker pool gates on (in-flight tasks
        // finish, no new ones start, the report still merges) → exit 130; a
        // second SIGINT force-exits 130. Without this, Ctrl-C killed the batch
        // hard and the merged report was never written.
        let cancel = Arc::new(AtomicBool::new(false));
        signal::install_sigint_cancel(cancel.clone());
        let code = batch::run(&cfg, dir, cli.jobs, &cli.report, cli.retries, cli.failed, cancel)?;
        std::process::exit(code);
    }

    // ── Single-run mode (headless or TUI) ─────────────────────────────
    let mut initial: Vec<Message> = Vec::new();
    let mut resumed = false;
    if cli.resume {
        let path = match session {
            Some(p) => p,
            // --resume with no persistence target is contradictory: there is
            // nothing to resume from. (Without --resume, --session none is
            // fine and just skips writing.)
            None => bail!("--resume needs a session file, but --session none disables persistence"),
        };
        if let Some((model, history)) = session::load(path)? {
            if model != cfg.model {
                eprintln!(
                    "[warn] session model '{model}' differs from config model '{}'",
                    cfg.model
                );
            }
            eprintln!(
                "resumed {} messages from {}",
                history.len(),
                cli.session.display()
            );
            initial = history;
            resumed = true;
        } else {
            eprintln!("[warn] --resume given but no session file found; starting fresh");
        }
    }

    // Append a task as a new user message. On resume, the config's task is
    // assumed already consumed — only an explicit --task sends a new one.
    // (cli.task was already merged into cfg.task above, so we check cli.task
    // directly to distinguish command-line from config.)
    let task_to_send = if resumed { cli_task.clone() } else { cfg.task.clone() };
    if let Some(task) = task_to_send {
        let mut blocks = vec![Block::Text(task)];
        for img in &cli.image {
            blocks.push(
                tools::load_block(img)
                    .with_context(|| format!("cannot load --image {}", img.display()))?,
            );
        }
        // If the resumed session already ends with a user message (e.g. it
        // crashed mid-turn at a tool_result), fold the new task into that
        // message instead of pushing another — two adjacent user messages
        // make Anthropic reject the request with a 400.
        if initial.last().map(|m| m.role) == Some(Role::User) {
            if let Some(last) = initial.last_mut() {
                last.content.extend(blocks);
            }
        } else {
            initial.push(Message::user(blocks));
        }
    } else if !cli.image.is_empty() {
        bail!("--image requires a task (--task or config \"task\")");
    }

    if !cli.tui {
        match initial.last() {
            Some(m) if m.role == Role::User => {}
            Some(_) => bail!(
                "session already ends with an assistant message; provide --task to continue"
            ),
            None => bail!("headless mode requires a task: set \"task\" in the config or pass --task"),
        }
    }

    // Whether the worker will auto-start a run on launch: it kicks off when
    // the session ends with a user message (an initial task, or a resumed
    // session stopped mid-turn at a tool_result). The TUI needs to know so it
    // can show "running" instead of letting the user submit into a queue.
    #[cfg_attr(not(feature = "tui"), allow(unused_variables))]
    let auto_start = initial.last().is_some_and(|m| m.role == Role::User);
    let client = provider::new_client(cfg.limits.timeout_secs);
    let mut agent = build_agent(&cfg, initial, &client)?;
    if let Some(path) = session {
        agent.enable_session(path.to_path_buf());
    }

    if cli.tui {
        #[cfg(feature = "tui")]
        {
            // Resume-aware: only an explicit --task is layered on top of
            // a resumed session; otherwise the config task (if any) is the
            // initial task. This is for display only — the worker kicks
            // off the run based on the agent's last message role, so we
            // never double-send the task.
            let initial_task = if resumed { cli_task.clone() } else { cfg.task.clone() };
            tui::run(agent, &cfg, initial_task, auto_start)
        }
        #[cfg(not(feature = "tui"))]
        {
            let _ = agent;
            bail!("this binary was built without the `tui` feature; rebuild with --features tui")
        }
    } else {
        let mut agent = agent;
        // Wire Ctrl-C into the graceful-cancel path (headless only): first
        // SIGINT flips a shared flag → the stream breaks within ~200ms →
        // result.json is still written (stop_reason=cancelled) → exit 130.
        // Second SIGINT exits at once. The TUI doesn't need this — crossterm
        // reads Ctrl-C as a key event in raw mode.
        let cancel = Arc::new(AtomicBool::new(false));
        agent.set_cancel(cancel.clone());
        signal::install_sigint_cancel(cancel);
        let code = harness::run(&mut agent, cli.output.as_deref(), cli.json, cli.retries)?;
        // Drop the agent before exit so MCP stdio children are killed via
        // StdioTransport::drop — std::process::exit skips destructors, which
        // would otherwise orphan spawned MCP server processes.
        drop(agent);
        std::process::exit(code);
    }
}

/// Wire together provider + tools (built-in whitelist + MCP servers) + agent.
pub(crate) fn build_agent(
    cfg: &Config,
    initial: Vec<Message>,
    client: &reqwest::blocking::Client,
) -> Result<Agent> {
    let workdir = cfg
        .workdir
        .clone()
        .unwrap_or(std::env::current_dir().context("cannot determine current directory")?);
    let api_key = cfg.api_key()?;
    let provider = provider::build(cfg, &api_key, client.clone());

    let mut tool_list = tools::build(&cfg.tools, cfg, client, &api_key)?;
    if !cfg.mcp_servers.is_empty() {
        tool_list.extend(tools::mcp::build(&cfg.mcp_servers)?);
    }

    let ctx = ToolCtx {
        workdir,
        max_output_chars: cfg.limits.max_output_chars,
        restrict_to_workdir: cfg.restrict_to_workdir,
        cancel: None,
    };
    Ok(Agent::new(cfg, provider, tool_list, initial, ctx))
}

/// Resolve the `--session` value into the persistence target, if any. The
/// literal `none` is the opt-out sentinel — no session file is read or written,
/// so a one-shot headless run doesn't litter the cwd. Anything else is treated
/// as a file path (the default is `.tinyagent-session.json`).
fn session_target(raw: &std::path::Path) -> Option<&std::path::Path> {
    if raw.as_os_str() == "none" {
        None
    } else {
        Some(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::session_target;
    use std::path::Path;

    #[test]
    fn session_target_none_disables_persistence() {
        assert!(session_target(Path::new("none")).is_none());
    }

    #[test]
    fn session_target_passes_real_paths_through() {
        // Default and custom paths (single-segment or nested) are kept as-is.
        assert_eq!(
            session_target(Path::new(".tinyagent-session.json")),
            Some(Path::new(".tinyagent-session.json"))
        );
        assert_eq!(
            session_target(Path::new("chat.json")),
            Some(Path::new("chat.json"))
        );
        assert_eq!(
            session_target(Path::new("/tmp/ta/chat.json")),
            Some(Path::new("/tmp/ta/chat.json"))
        );
    }
}
