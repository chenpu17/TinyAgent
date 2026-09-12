mod signal;
#[cfg(feature = "tui")]
mod tui;

// Core modules live in the `tiny_agent` library (src/lib.rs). Re-export them at
// this binary's crate root so the rest of main.rs — and `mod tui`, which reaches
// them as `crate::agent::…` — keeps resolving unchanged. The binary is only the
// process-level shell: arg parsing, SIGINT, exit codes, and the optional TUI.
pub(crate) use tiny_agent::{batch, build_agent, config, harness, provider, session, tools};
// `agent` itself is only reached by the TUI (`crate::agent::…`); gate it so a
// headless build doesn't warn about an unused re-export. The id helper below
// is used unconditionally (resume id pre-minting), so it imports directly.
#[cfg(feature = "tui")]
pub(crate) use tiny_agent::agent;
use tiny_agent::agent::numeric_id_suffix;

use anyhow::{bail, Context, Result};
use clap::Parser;
use config::Config;
use provider::{Block, Message, Role};
#[cfg(feature = "tui")]
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "tiny-agent",
    about = "A minimal single-binary AI agent: one JSON config, two modes"
)]
struct Cli {
    /// Path to the JSON config file. Optional: when omitted, the config is
    /// discovered via $TINYAGENT_CONFIG, then ~/.config/tinyagent/config.json
    /// (see `resolve_config_path`). Giving `--config` always uses exactly
    /// that path — it never falls back to a discovered default.
    #[arg(long)]
    config: Option<PathBuf>,

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

    /// Print the version and compiled-in feature set, then exit. Combine with
    /// `--json` for a machine-readable manifest that downstream CI can assert
    /// on (a rebuild that silently dropped `--features code` once caused a
    /// false regression in bench comparisons).
    #[arg(long)]
    version: bool,

    /// Batch mode: per-task workdir isolation. none (default) = all workers
    /// share the base workdir; copy = each task gets a throwaway copy (excludes
    /// target/node_modules/.git/results); git = each task gets a
    /// `git worktree` (workdir must be the repo root). git mode checks out
    /// HEAD: uncommitted/untracked files do NOT enter the isolate — commit
    /// them first or use copy.
    #[arg(long, default_value = "none")]
    isolate: String,
}

/// Compiled-in target triple approximation: `arch-os[-env]`, with the env
/// suffix only where it disambiguates the ABI (musl vs gnu matters for the
/// static-build deliverable; bare `macos`/`windows` would add noise). Enough
/// for CI to assert "this artifact is the musl static build".
fn target_label() -> String {
    let abi = if cfg!(target_env = "musl") {
        "musl"
    } else if cfg!(target_env = "gnu") {
        "gnu"
    } else if cfg!(target_env = "msvc") {
        "msvc"
    } else {
        ""
    };
    let base = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
    if abi.is_empty() {
        base
    } else {
        format!("{base}-{abi}")
    }
}

/// Compiled-in optional features, in stable order, so the manifest is diffable.
fn compiled_features() -> Vec<&'static str> {
    [
        cfg!(feature = "code").then_some("code"),
        cfg!(feature = "mock").then_some("mock"),
        cfg!(feature = "tui").then_some("tui"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// `--version` handler. Plain prints one human line; with `--json` prints a
/// machine-readable manifest (name / version / target / features) so a CI
/// pipeline can assert the binary carries the features the run expects
/// instead of discovering a silent degrade mid-benchmark.
fn print_version_manifest(json: bool) {
    let features = compiled_features();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "name": env!("CARGO_PKG_NAME"),
                "version": env!("CARGO_PKG_VERSION"),
                "target": target_label(),
                "features": features,
            })
        );
    } else {
        println!(
            "{} {} ({}) features: {}",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
            target_label(),
            if features.is_empty() {
                "none".to_string()
            } else {
                features.join(",")
            }
        );
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // `--version` answers before config resolution — CI asserting on the build
    // must not need a config file (or an API key) to exist.
    if cli.version {
        print_version_manifest(cli.json);
        return Ok(());
    }
    let cli_task = cli.task.clone();
    let config_path = resolve_config_path(cli.config.clone())?;
    let mut cfg = Config::load(&config_path)?;
    if let Some(t) = cli.task {
        // Reject an explicitly-empty --task "": an empty string would silently
        // mask a task set in the config (Some("") wins the overlay below) and
        // the agent would spin up with nothing to do.
        if t.trim().is_empty() {
            bail!("--task must not be empty");
        }
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
        if cli.resume {
            // --resume is single-run only; in batch mode each task is an
            // independent fresh run, so resuming is silently meaningless. Reject
            // explicitly rather than ignore (the --tui check above is the
            // precedent; this one was missing).
            bail!("--batch and --resume are mutually exclusive");
        }
        // Single-run-only flags that batch mode would silently ignore —
        // rejecting them beats a run that doesn't do what was asked.
        if !cli.image.is_empty() {
            bail!("--batch and --image are mutually exclusive (images attach to a single run's initial message)");
        }
        if cli.json {
            bail!("--batch and --json are mutually exclusive (batch writes the --report summary, not an NDJSON stream)");
        }
        if cli.output.is_some() {
            bail!("--batch and --output are mutually exclusive (per-task results go to the batch results dir)");
        }
        if cli_task.is_some() {
            // --task sets the single run's initial message; in batch mode every
            // task fragment carries its own task, and silently grafting --task
            // onto the base config would change ALL of them.
            bail!("--batch and --task are mutually exclusive (each task fragment defines its own task)");
        }
        // Same "differs from default" contract as the single-run checks
        // below — but via the equivalent-spelling helper, not bare Path::eq:
        // `./.tinyagent-session.json` is the default spelled out, and the
        // `none` opt-out sentinel persists NOTHING (so there's no session
        // semantics for batch to contradict). Both used to bail here — the
        // sentinel especially: `--batch --session none` was rejected as a
        // "custom session" even though it's the no-op spelling. A parent
        // beyond ""/"." IS custom.
        if !is_default_session(&cli.session) {
            // Batch tasks are independent fresh runs — session persistence has
            // nothing to resume, and per-task session files are not a thing.
            bail!("--batch and --session are mutually exclusive (batch tasks are independent fresh runs)");
        }
        // Wire Ctrl-C into batch the same way single-run headless does: first
        // SIGINT flips a shared flag the worker pool gates on (in-flight tasks
        // finish, no new ones start, the report still merges) → exit 130; a
        // second SIGINT force-exits 130. Without this, Ctrl-C killed the batch
        // hard and the merged report was never written.
        let cancel = Arc::new(AtomicBool::new(false));
        signal::install_sigint_cancel(cancel.clone());
        let isolate: batch::IsolateMode = cli
            .isolate
            .parse()
            .map_err(|e: String| anyhow::anyhow!("{e}"))?;
        let code = batch::run(
            &cfg,
            dir,
            cli.jobs,
            &cli.report,
            cli.retries,
            cli.failed,
            isolate,
            cancel,
        )?;
        std::process::exit(code);
    }

    // Single-run mode (headless or TUI) silently ignores batch-only flags.
    // Reject the ones a user might pass expecting them to take effect rather
    // than run a no-op that doesn't do what was asked — the symmetric reverse
    // of the batch-branch checks above. Detection is by "differs from default",
    // so passing the default value explicitly is a harmless no-op (not bailed).
    if cli.failed {
        bail!("--failed only applies to --batch mode");
    }
    if cli.jobs != 1 {
        bail!("--jobs only applies to --batch mode");
    }
    if !is_default_report(&cli.report) {
        bail!("--report only applies to --batch mode");
    }
    if cli.isolate != "none" {
        bail!("--isolate only applies to --batch mode");
    }

    // ── Single-run mode (headless or TUI) ─────────────────────────────
    let mut initial: Vec<Message> = Vec::new();
    let mut resumed = false;
    // Archive section of a resumed session (messages folded out of the live
    // history by earlier compactions) — restored onto the agent below so this
    // process's folds/prunes APPEND to it instead of overwriting it.
    let mut resumed_archive: Vec<Message> = Vec::new();
    if cli.resume {
        let path = match session {
            Some(p) => p,
            // --resume with no persistence target is contradictory: there is
            // nothing to resume from. (Without --resume, --session none is
            // fine and just skips writing.)
            None => bail!("--resume needs a session file, but --session none disables persistence"),
        };
        if let Some((model, history, archive)) = session::load_with_archive(path)? {
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
            resumed_archive = archive;
            resumed = true;
        } else {
            eprintln!("[warn] --resume given but no session file found; starting fresh");
        }
    }

    // Append a task as a new user message. On resume, the config's task is
    // assumed already consumed — only an explicit --task sends a new one.
    // (cli.task was already merged into cfg.task above, so we check cli.task
    // directly to distinguish command-line from config.)
    let task_to_send = if resumed {
        cli_task.clone()
    } else {
        cfg.task.clone()
    };
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
            let mut msg = Message::user(blocks);
            // Pre-mint the id above every id in history AND archive: the
            // agent's mint counter fills gaps by checking the live history
            // only, so a resumed session would otherwise re-issue the ids of
            // long-folded messages and collide with their archived
            // originals. Pre-assigning here keeps the mint inside the agent
            // unaware of the archive (assign_missing_ids keeps existing ids).
            let floor = initial
                .iter()
                .chain(resumed_archive.iter())
                .filter_map(|m| m.id.as_deref().and_then(numeric_id_suffix))
                .max()
                .unwrap_or(0);
            msg.id = Some(format!("m{}", floor + 1));
            initial.push(msg);
        }
    } else if !cli.image.is_empty() {
        bail!("--image requires a task (--task or config \"task\")");
    }

    // Decide TUI vs headless. Explicit `--tui` → TUI. An initial task (the
    // session ends with a user message) → headless. Otherwise a bare
    // invocation defaults to the TUI on an interactive terminal —
    // claude/codex-style "just run it" — but only in a `tui` build; a
    // non-interactive stdin (pipe/CI) keeps the old "needs a task" error so a
    // bare `tiny-agent` inside a script never blocks waiting for input. The
    // decision core is `default_tui_mode` (pure, unit-tested below).
    let tui_mode = if cli.tui {
        true
    } else {
        let last_is_user = initial.last().is_some_and(|m| m.role == Role::User);
        let has_initial = !initial.is_empty();
        #[cfg(feature = "tui")]
        let is_tty = std::io::stdin().is_terminal();
        #[cfg(not(feature = "tui"))]
        let is_tty = false; // irrelevant: tui_built=false makes the TUI arm unreachable
        #[cfg(feature = "tui")]
        let tui_built = true;
        #[cfg(not(feature = "tui"))]
        let tui_built = false;
        match default_tui_mode(last_is_user, has_initial, is_tty, tui_built) {
            TuiDefault::Headless => false,
            TuiDefault::Tui => true,
            TuiDefault::ErrAssistantTail => {
                bail!("session already ends with an assistant message; provide --task to continue")
            }
            TuiDefault::ErrNoTask => {
                #[cfg(feature = "tui")]
                {
                    bail!("headless mode requires a task: set \"task\" in the config or pass --task (stdin is not a tty, so the interactive TUI default did not apply)")
                }
                #[cfg(not(feature = "tui"))]
                {
                    bail!("headless mode requires a task: set \"task\" in the config or pass --task (or rebuild with --features tui for the interactive TUI default)")
                }
            }
        }
    };

    // Whether the worker will auto-start a run on launch: it kicks off when
    // the session ends with a user message (an initial task, or a resumed
    // session stopped mid-turn at a tool_result). The TUI needs to know so it
    // can show "running" instead of letting the user submit into a queue.
    #[cfg_attr(not(feature = "tui"), allow(unused_variables))]
    let auto_start = initial.last().is_some_and(|m| m.role == Role::User);
    // Snapshot the resumed history as TUI display rows BEFORE build_agent
    // consumes `initial` — the TUI rebuilds the transcript in scrollback from
    // this (display-only; the agent keeps the real history). Headless runs
    // skip the work.
    #[cfg(feature = "tui")]
    let resume_rows = if resumed && tui_mode {
        tui::transcript_rows(&initial)
    } else {
        Vec::new()
    };
    // Headless-only flags silently ignored in TUI: --json writes the NDJSON
    // stream and --output writes the result JSON (both happen in harness::run,
    // the non-TUI branch), --retries has no loop to wire into (each TUI
    // message is one run). Reject them BEFORE build_agent — it starts the
    // MCP server subprocesses, and an always-bail combo like `--tui --json`
    // was paying for their startup just to throw the flags error away.
    if tui_mode {
        if cli.json {
            bail!("--json only applies to headless (non-TUI) mode");
        }
        if cli.output.is_some() {
            bail!("--output only applies to headless (non-TUI) mode");
        }
        if cli.retries != 0 {
            // Each TUI message is a single run with its own (Esc/Ctrl-C)
            // cancellation — there's no transient-retry loop to wire into, so
            // a --retries value here would be silently ignored. Reject it
            // rather than let the user think retries are in effect.
            bail!("--retries only applies to headless (non-TUI) mode");
        }
    }
    let client = provider::new_client(cfg.limits.timeout_secs);
    let mut agent = build_agent(&cfg, initial, &client)?;
    if let Some(path) = session {
        agent.enable_session(path.to_path_buf());
    }
    agent.set_session_archive(resumed_archive);

    if tui_mode {
        #[cfg(feature = "tui")]
        {
            // Resume-aware: only an explicit --task is layered on top of
            // a resumed session; otherwise the config task (if any) is the
            // initial task. This is for display only — the worker kicks
            // off the run based on the agent's last message role, so we
            // never double-send the task.
            let initial_task = if resumed {
                cli_task.clone()
            } else {
                cfg.task.clone()
            };
            tui::run(agent, &cfg, initial_task, auto_start, resume_rows)
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
        // Headless Ctrl-C always precedes process exit, so the per-turn cancel
        // and the session-end teardown coincide here: arm the MCP shutdown-reaper
        // on the SAME flag so a Ctrl-C stuck inside an MCP call (blocked in the
        // 120s CALL_TIMEOUT, not at a turn boundary) still unblocks by killing
        // the server child — the run then ends and `drop(agent)` reaps cleanly.
        // (In the TUI these are SEPARATE flags: a per-turn Esc must NOT kill the
        // server the next turn reuses.)
        agent.set_shutdown(cancel.clone());
        signal::install_sigint_cancel(cancel);
        let code = harness::run(&mut agent, cli.output.as_deref(), cli.json, cli.retries)?;
        // Drop the agent before exit so MCP stdio children are killed via
        // StdioTransport::drop — std::process::exit skips destructors, which
        // would otherwise orphan spawned MCP server processes.
        drop(agent);
        std::process::exit(code);
    }
}

/// True when the `--report` value is the DEFAULT "report.json" — possibly via
/// an explicit-but-equivalent spelling. `Path::eq` compares components, so
/// `./report.json` != `report.json`; without this normalization a user
/// passing the explicit default spelling in single-run mode was bailed as if
/// they had chosen a custom report path. A parent beyond ""/"." IS custom.
fn is_default_report(p: &std::path::Path) -> bool {
    if p.file_name().and_then(|s| s.to_str()) != Some("report.json") {
        return false;
    }
    matches!(
        p.parent().and_then(|q| q.to_str()),
        None | Some("") | Some(".")
    )
}

/// True when the `--session` value leaves batch's semantics untouched: the
/// DEFAULT `.tinyagent-session.json` (possibly via an explicit-but-equivalent
/// spelling — `Path::eq` compares components, so `./x != x`; same
/// normalization as [`is_default_report`]) or the `none` opt-out sentinel
/// (persists nothing, so batch's "no session semantics" is already honored).
/// Anything else is a custom target the user chose — reject it rather than
/// silently ignore it.
fn is_default_session(p: &std::path::Path) -> bool {
    if p.as_os_str() == "none" {
        return true;
    }
    if p.file_name().and_then(|s| s.to_str()) != Some(".tinyagent-session.json") {
        return false;
    }
    matches!(
        p.parent().and_then(|q| q.to_str()),
        None | Some("") | Some(".")
    )
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

/// Outcome of deciding whether a bare (no `--tui`, no initial task) invocation
/// should default into the TUI. Pure (no env / stdin / cfg), so it is unit-
/// tested directly; the stdin probe and the `feature = "tui"` gate live in the
/// caller, which passes the results in as plain bools.
#[derive(Debug, PartialEq, Eq)]
enum TuiDefault {
    /// An initial task is present → run headless.
    Headless,
    /// No task, interactive tty, tui build → open the TUI.
    Tui,
    /// Resumed session ends with an assistant message — needs --task to go on.
    ErrAssistantTail,
    /// No task on a non-interactive stdin, or a binary built without `tui`.
    ErrNoTask,
}

fn default_tui_mode(
    last_is_user: bool,
    has_initial: bool,
    is_tty: bool,
    tui_built: bool,
) -> TuiDefault {
    if last_is_user {
        TuiDefault::Headless
    } else if has_initial {
        TuiDefault::ErrAssistantTail
    } else if is_tty && tui_built {
        TuiDefault::Tui
    } else {
        TuiDefault::ErrNoTask
    }
}

/// Resolve which config file to load, in priority order:
///
/// 1. `--config <path>` — returned as-is even when the file does not exist,
///    so `Config::load` reports the usual "cannot read" error instead of
///    silently substituting a default.
/// 2. `$TINYAGENT_CONFIG` (if set to a non-empty value).
/// 3. The XDG default (`default_config_path`) — but only if it exists.
/// 4. Otherwise a friendly error naming where to create one.
///
/// Thin env-reading shell over `choose_config_path` (the pure decision) and
/// `default_config_path` (the pure path builder), so all branching is covered
/// by env-free unit tests.
fn resolve_config_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let env_val = std::env::var(CONFIG_ENV).ok();
    let default = default_config_path();
    let default_exists = default.as_ref().is_some_and(|p| p.exists());
    choose_config_path(explicit, env_val, default, default_exists)
}

/// Name of the env var that can stand in for `--config` when it's omitted.
const CONFIG_ENV: &str = "TINYAGENT_CONFIG";

/// Pure decision core: `explicit` > `$TINYAGENT_CONFIG` > (default if it
/// exists). Tested without touching the real environment.
fn choose_config_path(
    explicit: Option<PathBuf>,
    env_val: Option<String>,
    default: Option<PathBuf>,
    default_exists: bool,
) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    if let Some(v) = env_val.filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(v));
    }
    if default_exists {
        if let Some(p) = default {
            return Ok(p);
        }
    }
    Err(missing_config_error(default.as_deref()))
}

/// `$XDG_CONFIG_HOME/tinyagent/config.json` when `XDG_CONFIG_HOME` is a
/// non-empty absolute path; otherwise `$HOME/.config/tinyagent/config.json`.
/// `None` when neither yields a base (e.g. `$HOME` unset). Reads the real
/// environment; the pure, testable core is `default_config_path_in`.
fn default_config_path() -> Option<PathBuf> {
    default_config_path_in(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

fn default_config_path_in(xdg: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let base = match xdg {
        // Per the XDG spec, a relative or empty XDG_CONFIG_HOME is ignored.
        Some(x) if !x.is_empty() && Path::new(x).is_absolute() => PathBuf::from(x),
        _ => home
            .filter(|h| !h.is_empty())
            .map(|h| PathBuf::from(h).join(".config"))?,
    };
    Some(base.join("tinyagent").join("config.json"))
}

/// Friendly error for the "no config found" case. `default` is the path we
/// looked at (and would have used had it existed), if any. Rendered as a
/// static `concat!` body so the JSON braces and indentation are literal, not
/// at the mercy of `format!`/`\`-continuation escaping.
fn missing_config_error(default: Option<&Path>) -> anyhow::Error {
    let where_to = default
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.config/tinyagent/config.json".to_string());
    const BODY: &str = concat!(
        "Minimal example (OpenAI-compatible endpoint — e.g. GLM, DeepSeek, vLLM):\n",
        "  {\n",
        "    \"provider\": \"openai\",\n",
        "    \"base_url\": \"https://your-gateway/v1\",\n",
        "    \"api_key_env\": \"YOUR_API_KEY\",\n",
        "    \"model\": \"your-model\",\n",
        "    \"max_tokens\": 8192\n",
        "  }\n",
        "\n",
        "Then export the key the config points at and launch:\n",
        "  export YOUR_API_KEY=...\n",
        "  tiny-agent --tui\n",
        "\n",
        "Or point at a file directly with --config, or set $TINYAGENT_CONFIG.",
    );
    anyhow::anyhow!(
        "No config file found.\nCreate one at:\n  {where_to}\n\n{body}",
        body = BODY,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        choose_config_path, default_config_path_in, default_tui_mode, is_default_report,
        is_default_session, session_target, TuiDefault,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn is_default_report_accepts_equivalent_spellings() {
        // C5: `Path::eq` compares components, so "./report.json" !=
        // "report.json" — the old literal compare bailed single-run users who
        // spelled the default explicitly. Equivalent spellings are no-ops;
        // anything with a real parent/name is custom.
        for p in ["report.json", "./report.json", ".//report.json"] {
            assert!(is_default_report(Path::new(p)), "{p:?} is the default");
        }
        for p in [
            "out/report.json",
            "report.json.bak",
            "other.json",
            "sub/./report.json",
        ] {
            assert!(!is_default_report(Path::new(p)), "{p:?} is custom");
        }
    }

    #[test]
    fn is_default_session_accepts_sentinel_and_equivalent_spellings() {
        // The batch gate used bare Path::eq against the default, so BOTH an
        // explicit-but-equivalent spelling AND the `none` opt-out sentinel
        // were rejected as "custom". Sentinel + equivalent defaults must pass;
        // a real custom target must still fail.
        for p in [
            ".tinyagent-session.json",
            "./.tinyagent-session.json",
            "none",
        ] {
            assert!(
                is_default_session(Path::new(p)),
                "{p:?} leaves batch semantics untouched"
            );
        }
        for p in [
            "chat.json",
            "out/.tinyagent-session.json",
            ".tinyagent-session.json.bak",
            "nonexistent/none",
        ] {
            assert!(!is_default_session(Path::new(p)), "{p:?} is custom");
        }
    }

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

    #[test]
    fn choose_explicit_config_wins() {
        // An explicit path is returned as-is even if it doesn't exist; env and
        // default are never consulted.
        let p = choose_config_path(
            Some(PathBuf::from("/explicit.json")),
            Some("/from/env.json".into()),
            Some(PathBuf::from("/default.json")),
            true,
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/explicit.json"));
    }

    #[test]
    fn choose_env_when_no_explicit() {
        let p = choose_config_path(
            None,
            Some("/from/env.json".into()),
            Some(PathBuf::from("/default.json")),
            true,
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/from/env.json"));
    }

    #[test]
    fn choose_empty_env_falls_through_to_default() {
        // An empty $TINYAGENT_CONFIG is treated as unset (never an empty path).
        let p = choose_config_path(
            None,
            Some(String::new()),
            Some(PathBuf::from("/d.json")),
            true,
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/d.json"));
    }

    #[test]
    fn choose_default_only_when_it_exists() {
        let p = choose_config_path(None, None, Some(PathBuf::from("/d.json")), true).unwrap();
        assert_eq!(p, PathBuf::from("/d.json"));
    }

    #[test]
    fn choose_errors_when_default_missing() {
        // The default path was located but the file doesn't exist → friendly
        // error naming exactly that path so the user knows where to create it.
        let err =
            choose_config_path(None, None, Some(PathBuf::from("/dft.json")), false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("No config file found"), "{msg}");
        assert!(msg.contains("/dft.json"), "{msg}");
    }

    #[test]
    fn choose_errors_when_no_default_locatable() {
        // No $HOME → no default path at all. The message still names the
        // conventional location so the user knows what to aim for.
        let err = choose_config_path(None, None, None, false).unwrap_err();
        assert!(err.to_string().contains("config file"), "{}", err);
    }

    #[test]
    fn default_path_prefers_absolute_xdg() {
        // `is_absolute()` is platform-dependent: a `/`-rooted path is absolute
        // on unix but NOT on Windows (which needs a drive letter / UNC). Pass a
        // host-absolute path or the test hits the home-fallback branch on
        // Windows and the assertion can never hold. Build the expected path
        // with join() so the assertion is separator-agnostic.
        let xdg = if cfg!(unix) {
            "/custom/xdg"
        } else {
            "C:/custom/xdg"
        };
        let p = default_config_path_in(Some(xdg), Some("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from(xdg).join("tinyagent").join("config.json"));
    }

    #[test]
    fn default_path_ignores_relative_or_empty_xdg() {
        // A relative (or empty) XDG_CONFIG_HOME is ignored per the XDG spec and
        // falls back to $HOME/.config.
        for xdg in ["relative/xdg", ""] {
            let p = default_config_path_in(Some(xdg), Some("/home/u")).unwrap();
            assert_eq!(
                p,
                PathBuf::from("/home/u/.config/tinyagent/config.json"),
                "{xdg:?}"
            );
        }
    }

    #[test]
    fn default_path_falls_back_to_home() {
        let p = default_config_path_in(None, Some("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.config/tinyagent/config.json"));
    }

    #[test]
    fn default_path_none_without_home_or_xdg() {
        // No usable XDG and no $HOME → cannot locate a default.
        assert!(default_config_path_in(None, None).is_none());
    }

    #[test]
    fn tui_default_explicit_task_is_headless() {
        // An initial task (session ends with a user message) always runs
        // headless regardless of tty — the task is what was asked to run.
        assert_eq!(
            default_tui_mode(true, true, true, true),
            TuiDefault::Headless
        );
        assert_eq!(
            default_tui_mode(true, true, false, false),
            TuiDefault::Headless
        );
    }

    #[test]
    fn tui_default_resumed_assistant_tail_errors() {
        // Resumed session ends with an assistant message: nothing to run
        // without an explicit --task.
        assert_eq!(
            default_tui_mode(false, true, true, true),
            TuiDefault::ErrAssistantTail
        );
    }

    #[test]
    fn tui_default_bare_on_tty_opens_tui() {
        // claude/codex-style: a bare invocation on an interactive terminal
        // opens the TUI (when the binary was built with the tui feature).
        assert_eq!(default_tui_mode(false, false, true, true), TuiDefault::Tui);
    }

    #[test]
    fn tui_default_bare_non_tty_errors() {
        // Script/pipe safety: a bare invocation with non-interactive stdin must
        // NOT block on the TUI — it errors so a stray `tiny-agent` in CI fails
        // loudly instead of hanging.
        assert_eq!(
            default_tui_mode(false, false, false, true),
            TuiDefault::ErrNoTask
        );
    }

    #[test]
    fn tui_default_bare_without_tui_build_errors() {
        // A binary built without the tui feature can't open the TUI even on a
        // tty; guide the user toward headless (or a rebuild).
        assert_eq!(
            default_tui_mode(false, false, true, false),
            TuiDefault::ErrNoTask
        );
    }
}
