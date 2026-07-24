mod agent;
mod batch;
mod config;
mod harness;
mod mcp;
mod provider;
mod session;
mod tools;
#[cfg(feature = "tui")]
mod tui;

use agent::Agent;
use anyhow::{bail, Context, Result};
use clap::Parser;
use config::Config;
use provider::{Block, Message, Role};
use std::path::PathBuf;
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

    /// Write a structured result JSON here when done (headless mode)
    #[arg(long)]
    output: Option<PathBuf>,

    /// Attach an image to the initial message (repeatable)
    #[arg(long)]
    image: Vec<PathBuf>,

    /// Session file for conversation persistence
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cli_task = cli.task.clone();
    let mut cfg = Config::load(&cli.config)?;
    if let Some(t) = cli.task {
        cfg.task = Some(t);
    }

    // ── Batch mode ────────────────────────────────────────────────────
    if let Some(dir) = &cli.batch {
        if cli.tui {
            bail!("--batch and --tui are mutually exclusive");
        }
        let code = batch::run(&cfg, dir, cli.jobs, &cli.report)?;
        std::process::exit(code);
    }

    // ── Single-run mode (headless or TUI) ─────────────────────────────
    let mut initial: Vec<Message> = Vec::new();
    let mut resumed = false;
    if cli.resume {
        if let Some((model, history)) = session::load(&cli.session)? {
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
    let auto_start = initial.last().is_some_and(|m| m.role == Role::User);
    let client = provider::new_client(cfg.limits.timeout_secs);
    let mut agent = build_agent(&cfg, initial, &client)?;
    agent.enable_session(cli.session.clone());

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
        let code = harness::run(&mut agent, cli.output.as_deref())?;
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

    let mut tool_list = tools::build(&cfg.tools)?;
    if !cfg.mcp_servers.is_empty() {
        tool_list.extend(tools::mcp::build(&cfg.mcp_servers)?);
    }

    let ctx = ToolCtx {
        workdir,
        max_output_chars: cfg.limits.max_output_chars,
        restrict_to_workdir: cfg.restrict_to_workdir,
    };
    Ok(Agent::new(cfg, provider, tool_list, initial, ctx))
}
