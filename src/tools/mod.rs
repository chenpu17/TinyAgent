mod document;
mod file;
mod image;
pub mod mcp;
// The wasm-sandboxed JS tool. Gated: wasmtime adds ≈4.6 MiB to the release
// binary, and enabling the tool is a per-deployment trust decision
// (run_code_design.md §15). Tests
// live at the bottom of the file, same gate.
#[cfg(feature = "code")]
pub mod run_code;
mod shell;
mod skill;
mod subagent;
pub(crate) mod todo;

pub use document::load_document_block;
pub use image::load_block;
#[cfg(feature = "code")]
pub use run_code::RunCodeTool;
pub use skill::{ListSkill, ReadSkill};

use crate::agent::AgentEvent;
use crate::config::Config;
use crate::provider::{Block, ToolDef, Usage};
use anyhow::{bail, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Shared context passed to every tool invocation.
pub struct ToolCtx {
    pub workdir: PathBuf,
    pub max_output_chars: usize,
    /// When true, file tools refuse paths that escape `workdir`.
    pub restrict_to_workdir: bool,
    /// Cancellation flag shared with the agent run. Pollable tools (shell)
    /// check this between waits so Ctrl-C can interrupt a long-running
    /// command within ~50ms instead of waiting out its full timeout. Wired in
    /// both headless (`main` installs the SIGINT flag and `set_cancel`s it)
    /// and the TUI (Ctrl-C during a run); None only in tests.
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Max wall-clock seconds for one `shell` command (`limits.shell_timeout_secs`).
    /// `0` = no per-command limit — the cancel flag above still applies, and
    /// the run-level `timeout_secs` remains the backstop.
    pub shell_timeout_secs: u64,
    /// Third-party interception layer ([`crate::hooks::Hooks`]), installed via
    /// `Agent::set_hooks` (assembly leaves it None). Shared by Arc with `task`
    /// sub-agents — the child's ctx clones it, so a tool denied at the top
    /// level stays denied when reached through a sub-agent (no bypass).
    pub hooks: Option<std::sync::Arc<dyn crate::hooks::Hooks>>,
    /// When this ctx belongs to a `task` sub-agent, its name (the same
    /// identity that tags its forwarded events) — lets hooks tell top-level
    /// calls from sub-agent calls. None on the main agent.
    pub subagent: Option<String>,
    /// Run-wide token budget gate shared by the parent agent and EVERY `task`
    /// sub-agent of the current run (`max_total_tokens > 0` only). Each agent
    /// `fetch_add`s its OWN direct chat spend (turn chats + compression
    /// summaries); the budget gates — parent and children alike — read this
    /// SAME gate, so N parallel sub-agents collectively trip the cap instead
    /// of each independently burning a full budget before the parent's
    /// join-time fold (the old "事后熔断" hole). Installed per run by
    /// `Agent::run` (fresh gate — usage totals also reset per run);
    /// `run_as_subagent` keeps the inherited one. The sandbox's
    /// `tools.budget()` reads it to report `tokens_left`. IMPORTANT for
    /// callers: when the parent folds a sub-agent's `ToolOutput.usage` into
    /// its own totals (accounting), it must NOT also add here — the child
    /// already counted its direct spend (double-add would halve the
    /// effective budget).
    pub budget: Option<BudgetGate>,
    /// The run's wall-clock deadline (`limits.timeout_secs` from run start),
    /// published so a `task` sub-agent can clamp its own timeout to the
    /// parent's REMAINING time instead of a fresh full copy (a child spawned
    /// near the deadline would otherwise run a whole second `timeout_secs`,
    /// letting the fleet take ~2× the configured budget). SOFT limit: it
    /// bounds chat requests and turn starts (and, via this field, sub-agent
    /// turn starts) — tool execution is bounded by each tool's own limits
    /// (`shell_timeout_secs`, the MCP call timeout), and host callbacks in
    /// the language bindings cannot be interrupted at all.
    pub deadline: Option<std::time::Instant>,
}

/// The run-level token-budget gate (`limits.max_total_tokens`): the fleet-wide
/// spend counter PLUS the cap it gates against, so a consumer that only sees
/// the [`ToolCtx`] — the run_code sandbox's `tools.budget()` — can report
/// REMAINING headroom without re-deriving the cap from config. One gate per
/// top-level run (`Agent::run` installs a fresh one); `task` sub-agents and
/// the sandbox clone it (Arc-backed `used`), so parent, children, and
/// in-flight guest calls all read/write the same numbers.
#[derive(Clone)]
pub struct BudgetGate {
    /// Saturating-accumulated billed tokens. Each agent bills its OWN direct
    /// chats (turn chats + compression summaries); tool-billed usage folds
    /// into totals OUTSIDE the gate (see the `budget` field doc on ToolCtx).
    pub used: Arc<std::sync::atomic::AtomicU64>,
    /// `limits.max_total_tokens`. A gate is only installed when the cap is
    /// > 0 — 0 means "no limit" and leaves [`ToolCtx::budget`] None.
    pub cap: u64,
}

impl BudgetGate {
    pub fn new(cap: u64) -> Self {
        Self {
            used: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            cap,
        }
    }

    /// Remaining headroom, saturating: between a turn's billing and the next
    /// turn-boundary gate check the counter may legitimately sit ABOVE the
    /// cap (the gate stops the NEXT turn, it never kills the running one) —
    /// report 0 there, never a wrapped huge number.
    pub fn tokens_left(&self) -> u64 {
        self.cap
            .saturating_sub(self.used.load(std::sync::atomic::Ordering::Relaxed))
    }
}

#[derive(Debug)]
pub struct ToolOutput {
    pub content: String,
    pub image: Option<Block>,
    pub is_error: bool,
    /// Token usage a tool wants billed to the parent run. Leaf tools (shell,
    /// file, …) leave this None; the `task` sub-agent tool sets it to its
    /// child run's usage so sub-agent cost is counted honestly in the parent's
    /// totals (and surfaces via AgentEvent::Usage / result.json).
    pub usage: Option<Usage>,
    /// Structured audit metadata for the result file, NOT shown to the model —
    /// the model only sees `content`. Currently used by the `task` sub-agent to
    /// attach a compact trace (turns / tools used / token subtotal / stop
    /// reason) so `result.json` is self-contained for auditing parallel work
    /// without polluting the parent's context. None for every leaf tool. Lands
    /// in `ToolCallRecord.meta` → `result.json` `tool_calls[].meta`.
    pub meta: Option<Value>,
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: s.into(),
            image: None,
            is_error: false,
            usage: None,
            meta: None,
        }
    }

    pub fn error(s: impl Into<String>) -> Self {
        Self {
            content: s.into(),
            image: None,
            is_error: true,
            usage: None,
            meta: None,
        }
    }
}

/// Whether the agent loop may execute several calls of one tool CONCURRENTLY.
///
/// When an assistant turn contains multiple tool_use blocks, the loop splits
/// them by this marker: `Sequential` calls run one at a time in block order
/// (the default — safe for file/shell tools whose side effects would race),
/// while every `Parallel` call is deferred and fanned out on its own scoped
/// thread (the parallel batch in `agent::run_impl`). Routing is by this
/// marker, NOT by tool name — a third-party tool opting into `Parallel` gets
/// the same fan-out the built-in `task` tool does.
///
/// `#[non_exhaustive]`: variants may be added later; external `match`es must
/// keep a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolConcurrency {
    /// Calls run one at a time, in block order. The default: correct for any
    /// tool whose per-call side effects could race (files, shells, ...).
    Sequential,
    /// Multiple calls in the same turn may run concurrently, each on its own
    /// thread. An implementation opting in TAKES ON two obligations:
    /// - reentrancy — `run`/`run_with_emit` may be executing simultaneously on
    ///   several threads (all on the same `&self`), so no unsynchronized
    ///   shared mutation;
    /// - cancel responsiveness — poll `ctx.cancel` between waits (like
    ///   `shell` does) so Ctrl-C stops an in-flight call instead of waiting
    ///   it out.
    Parallel,
}

/// Validate a tool list's names: non-empty, `[A-Za-z0-9_-]` only, ≤64 chars
/// (the Anthropic tool-name / OpenAI function-name wire limit), and unique.
/// Called from BOTH assembly ([`crate::assemble`], covering built-in + MCP +
/// skill + extra tools) and [`crate::agent::Agent::try_new`] (covering direct
/// construction — the README's "full control" path), so no entry point can
/// build an agent whose dispatch silently shadows a duplicate or whose names
/// only fail later as a provider 400.
pub(crate) fn validate_tool_names(tools: &[Box<dyn Tool>]) -> Result<()> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for t in tools {
        let name = t.def().name;
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
        // run_code's `tools.list()` rides the host_call_tool import under this
        // reserved name — a same-named real tool would be unreachable from the
        // sandbox while LOOKING available (silent shadowing by a string). The
        // check is feature-gated: without `code` the reserved name is inert.
        #[cfg(feature = "code")]
        if name == "__tools_list" {
            bail!(
                "tool name '__tools_list' is reserved (run_code's tools.list \
                 channel) — pick another name"
            );
        }
        if !seen.insert(name.clone()) {
            bail!(
                "duplicate tool name '{name}': built-in, MCP, and custom tools \
                 must be unique — the later definition would be silently shadowed \
                 at dispatch (first match wins)"
            );
        }
    }
    Ok(())
}

pub trait Tool: Send + Sync {
    fn def(&self) -> ToolDef;
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput>;
    /// Run with an event sink, for tools that stream progress into the parent
    /// run (the `task` sub-agent forwards its child's `AgentEvent`s, wrapped
    /// with the sub-agent's identity). Leaf tools have nothing to emit and use
    /// the default, which ignores `emit` and delegates to `run` — so adding
    /// this didn't touch the 6 leaf impls. The agent loop always calls this;
    /// the parallel path (commit 2) gives each worker its own `emit` closure
    /// backed by an mpsc sender.
    fn run_with_emit(
        &self,
        input: &Value,
        ctx: &ToolCtx,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<ToolOutput> {
        let _ = emit;
        self.run(input, ctx)
    }

    /// Receive a SEPARATE shutdown flag, distinct from the run's per-turn
    /// `cancel` flag. Tools that own un-interruptible blocking resources (an MCP
    /// stdio server's child process) override this to arm a watcher that tears
    /// the resource down on **session end** — so a stuck call unblocks (the
    /// worker can then drop the agent) instead of leaving the resource orphaned
    /// when the frontend gives up waiting. Default no-op for leaf tools with
    /// nothing to tear down.
    ///
    /// Why a separate flag from `cancel`: `cancel` is also raised on a
    /// recoverable per-turn interrupt (TUI Esc / Ctrl-C), where the next turn
    /// reuses the same resource. Tearing down (e.g. killing the MCP server) is
    /// irreversible there, so teardown must be armed on `shutdown`, which the
    /// frontend sets ONLY on session end. The per-turn `cancel` flag is wired
    /// separately via `ToolCtx` and never reaches this method.
    fn set_shutdown(&mut self, _shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>) {}

    /// Concurrency class for multiple calls of THIS tool in one turn (see
    /// [`ToolConcurrency`]). Default `Sequential` (the safe side); only tools
    /// built for concurrent execution override it (the `task` sub-agent — each
    /// call is an independent child agent). The loop reads this through the
    /// boxed trait object, so it is a method — an associated const would not
    /// be object-safe on `dyn Tool`.
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Sequential
    }

    /// Whether a `task` sub-agent may carry this tool in its own tool list.
    /// Default true. Overridden false where inheritance is wrong: `task`
    /// itself (no recursion) and `todo` (plan tracking belongs to the parent;
    /// a child's calls would never reach the parent's `result.json.plan`
    /// anyway). The sub-agent's list is built full, then retained by this
    /// marker — NOT filtered by name — so a third-party tool can exclude
    /// itself the same way.
    fn available_to_subagent(&self) -> bool {
        true
    }

    /// Whether the `run_code` sandbox's guest registry may carry this tool
    /// (`tools.call` from LLM-written JS). **Default false — the guest
    /// surface is opt-in**, its own capability axis distinct from
    /// [`Self::available_to_subagent`] (a tool can be right for child agents
    /// yet wrong to expose to generated code, or vice versa). The built-ins
    /// the sandbox is built around — file 四件套, shell, shared MCP tools —
    /// override true; `task`/`todo`/run_code/skill tools and every
    /// unannotated third-party tool stay out. Host callbacks
    /// ([`CallbackTool`]) opt in per instance via
    /// [`CallbackTool::with_code_access`]: a callback runs arbitrary host
    /// code, so sandbox visibility must be an explicit authorization, not a
    /// side effect of registering it top-level.
    fn available_to_code(&self) -> bool {
        false
    }

    /// The MCP server (CONFIG name, unsanitized) this tool belongs to, for
    /// `Agent::remove_mcp_server`'s hot-remove routing. Default `None` for
    /// every non-MCP tool — capability-marker style (like `concurrency()`):
    /// no downcasting and no display-name prefix parsing on the agent side.
    fn mcp_server(&self) -> Option<&str> {
        None
    }
}

/// A tool backed by a closure — the zero-boilerplate way to add a custom tool,
/// and the shared base the Python / TypeScript bindings build on. The closure
/// receives the model's parsed JSON arguments and the shared [`ToolCtx`], and
/// returns a [`ToolOutput`].
///
/// Bindings don't implement [`Tool`] directly: each wraps its host-language
/// callable into a closure that handles the GIL / threadsafe-function dance,
/// then hands that closure here so the `Tool` impl is written once. A plain
/// Rust user can also use it to make a tool without defining a struct:
///
/// ```no_run
/// # use tiny_agent::{CallbackTool, ToolDef};
/// # use serde_json::json;
/// let def = ToolDef {
///     name: "echo".into(),
///     description: "echo back the argument".into(),
///     schema: json!({"type": "object", "properties": {"msg": {"type": "string"}}}),
/// };
/// let tool = CallbackTool::new(def, |input, _ctx| {
///     Ok(tiny_agent::ToolOutput::text(input["msg"].as_str().unwrap_or("").to_string()))
/// });
/// ```
/// Boxed closure backing a [`CallbackTool`]. Factored into an alias so the
/// `+ Send + Sync` bounds don't trip clippy's `type_complexity` and so the
/// bindings can name the same signature if they ever expose it directly.
pub type ToolFn = Box<dyn Fn(&Value, &ToolCtx) -> Result<ToolOutput> + Send + Sync>;

pub struct CallbackTool {
    def: ToolDef,
    func: ToolFn,
    /// Opt-in to the run_code guest registry (see [`Tool::available_to_code`]
    /// — default closed; set via [`CallbackTool::with_code_access`]).
    code_access: bool,
}

impl CallbackTool {
    pub fn new<F>(def: ToolDef, func: F) -> Self
    where
        F: Fn(&Value, &ToolCtx) -> Result<ToolOutput> + Send + Sync + 'static,
    {
        Self {
            def,
            func: Box::new(func),
            code_access: false,
        }
    }

    /// Expose THIS tool to `tools.call` inside the run_code sandbox (the
    /// default is closed — see [`Tool::available_to_code`]). Builder-style:
    /// `CallbackTool::new(def, f).with_code_access()`. The callback then
    /// runs LLM-written arguments under the same hooks gate and call-quota
    /// accounting as every other guest call — authorizing it is authorizing
    /// generated code to invoke host code, so keep it deliberate.
    pub fn with_code_access(mut self) -> Self {
        self.code_access = true;
        self
    }
}

impl Tool for CallbackTool {
    fn def(&self) -> ToolDef {
        self.def.clone()
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        (self.func)(input, ctx)
    }
    fn available_to_code(&self) -> bool {
        self.code_access
    }
}

/// Arc-shared handle to a host-injected tool so the SAME instance serves
/// both the agent's top-level list and (on the tool's own
/// [`Tool::available_to_code`] opt-in) the run_code guest registry — host
/// callbacks keep single-instance state instead of being duplicated. Every
/// `&self` trait method delegates; `set_shutdown` can only reach the inner
/// tool while the Arc is still exclusive (it never is once both lists hold a
/// handle), so in practice it is a no-op — realistic injected tools
/// (`CallbackTool`) have nothing to tear down, and a tool needing
/// session-end teardown should not be injected as a shared handle.
pub(crate) struct SharedTool {
    pub(crate) inner: Arc<Box<dyn Tool>>,
}

impl Tool for SharedTool {
    fn def(&self) -> ToolDef {
        self.inner.def()
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        self.inner.run(input, ctx)
    }
    fn run_with_emit(
        &self,
        input: &Value,
        ctx: &ToolCtx,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<ToolOutput> {
        self.inner.run_with_emit(input, ctx, emit)
    }
    fn concurrency(&self) -> ToolConcurrency {
        self.inner.concurrency()
    }
    fn available_to_subagent(&self) -> bool {
        self.inner.available_to_subagent()
    }
    fn available_to_code(&self) -> bool {
        self.inner.available_to_code()
    }
    fn mcp_server(&self) -> Option<&str> {
        self.inner.mcp_server()
    }
}

/// Instantiate the built-in tools named in the config whitelist.
///
/// `cfg` / `client` / `api_key` are only used by the `task` sub-agent tool
/// (which builds a fresh child provider+agent); the leaf tools ignore them.
/// Passing them here keeps the `Tool` trait's `run(ctx)` signature free of
/// agent-construction plumbing.
pub fn build(
    names: &[String],
    cfg: &Config,
    client: &reqwest::blocking::Client,
    api_key: &str,
    shared_mcp: &[mcp::McpTool],
) -> Result<Vec<Box<dyn Tool>>> {
    let mut out: Vec<Box<dyn Tool>> = Vec::new();
    for name in names {
        out.push(match name.as_str() {
            "shell" => Box::new(shell::Shell),
            "read_file" => Box::new(file::ReadFile),
            "write_file" => Box::new(file::WriteFile),
            "edit_file" => Box::new(file::EditFile),
            "list_dir" => Box::new(file::ListDir),
            "load_image" => Box::new(image::LoadImage),
            "todo" => Box::new(todo::Todo),
            // The sub-agent clones the SHARED MCP tools into each child
            // (Arc-sharing the connections) instead of re-spawning every MCP
            // server per task call — see SubAgent::run_with_emit.
            "task" => Box::new(subagent::SubAgent::new(
                cfg.clone(),
                client.clone(),
                api_key.to_string(),
                shared_mcp.to_vec(),
            )),
            // Registry / sub-agent paths only (design §8.1): assembly strips
            // `run_code` from the whitelist it builds (the top-level instance
            // comes solely from `assemble`'s single construction point), so a
            // `run_code` entry reaches here only via a child registry build —
            // where available_to_subagent() filters it straight back out. The
            // nested instance is constructed-and-dropped, which is cheap (a
            // four-field snapshot; the engine is a process-level OnceLock).
            #[cfg(feature = "code")]
            "run_code" => Box::new(crate::tools::run_code::RunCodeTool::new(
                cfg,
                std::sync::Arc::new(std::sync::RwLock::new(cfg.code.clone())),
                client,
                api_key,
                shared_mcp.to_vec(),
                vec![],
            )),
            other => {
                // Feature-off + explicit listing: fail with the remedy, not
                // just "unknown tool" (design §8.1 — the tool EXISTS, the
                // build just doesn't carry it).
                if other == "run_code" {
                    bail!(
                        "unknown tool in config: run_code — the sandboxed JS \
                         tool needs the `code` cargo feature; rebuild with \
                         --features code"
                    );
                }
                bail!("unknown tool in config: {other}")
            }
        });
    }
    Ok(out)
}

/// Truncate to `max` chars on a char boundary, noting the omission.
pub(crate) fn truncate(s: &mut String, max: usize) {
    if s.chars().count() > max {
        let byte_idx = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
        s.truncate(byte_idx);
        s.push_str(&format!("\n... [truncated, limit {max} chars]"));
    }
}

/// Resolve a user-supplied path against the working directory. When
/// `restrict_to_workdir` is on, paths escaping the workdir (via `../`
/// or absolute paths) are rejected.
pub(crate) fn resolve(ctx: &ToolCtx, path: &str) -> Result<PathBuf> {
    let p = PathBuf::from(path);
    let full = if p.is_absolute() {
        p
    } else {
        ctx.workdir.join(p)
    };
    if ctx.restrict_to_workdir {
        return confine(ctx, &full);
    }
    Ok(full)
}

/// Check that `full` stays inside `root`. Canonicalizes `root` first, then the
/// deepest existing ancestor of `full` (so writes to not-yet-existing files are
/// checked too) and re-appends the remainder; a plain prefix comparison against
/// the canonicalized root is then sound. `label` is plugged into the escape
/// error message (e.g. "working directory", "skill directory") so the user
/// knows which boundary was crossed.
///
/// Shared by the workdir sandbox (`confine`, via [`ToolCtx::workdir`]) and the
/// per-skill-directory sandbox (`read_skill`/`list_skill` file reads), which is
/// why the root is taken as a parameter rather than read off `ToolCtx`.
///
/// **Best-effort sandbox, not a hard security boundary.** A classic TOCTOU
/// window exists between this check and the caller's actual I/O: a path
/// component swapped for a symlink pointing outside `root` during that gap
/// would slip through. This exists to keep the *model* from accidentally
/// straying outside the allowed directory (its own `../` or absolute paths),
/// not to resist an adversarial process with concurrent filesystem access. A
/// TOCTOU-hardened version (openat / `O_NOFOLLOW` directory-fd walk) would need
/// platform-specific code on Windows and is out of scope for this threat model.
pub(crate) fn confined_to(root: &Path, full: &Path, label: &str) -> Result<PathBuf> {
    let root_c = root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot canonicalize root {}: {}", root.display(), e))?;
    let mut ancestor: &Path = full;
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(mut resolved) => {
                for comp in rest.iter().rev() {
                    resolved.push(comp);
                }
                if resolved.starts_with(&root_c) {
                    return Ok(resolved);
                }
                bail!("path escapes the {label}: {}", full.display());
            }
            Err(_) => match ancestor.file_name() {
                Some(name) => {
                    rest.push(name.to_os_string());
                    ancestor = ancestor.parent().ok_or_else(|| {
                        anyhow::anyhow!("cannot resolve path: {}", full.display())
                    })?;
                }
                None => bail!("cannot resolve path: {}", full.display()),
            },
        }
    }
}

/// Check that `full` stays inside the workdir. Thin wrapper over [`confined_to`]
/// for the workdir-rooted sandbox used by `restrict_to_workdir`. The workdir is
/// canonicalized at config load, and `confined_to` canonicalizes it again
/// (idempotent), so the prefix comparison is sound.
fn confine(ctx: &ToolCtx, full: &Path) -> Result<PathBuf> {
    confined_to(
        &ctx.workdir,
        full,
        "working directory (restrict_to_workdir)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test 13's feature-off half: listing `run_code` without the `code`
    /// cargo feature is an actionable error, not a silent gap. Only compiled
    /// (and run) on feature-off test invocations — the feature-ON behavior
    /// (construction succeeds) is covered in run_code.rs's tests.
    #[cfg(not(feature = "code"))]
    #[test]
    fn run_code_listing_errors_without_feature() {
        let cfg: crate::config::Config = serde_json::from_str(
            "{\"provider\":\"openai\",\"api_key_env\":\"K\",\"model\":\"m\",\
             \"inject_environment\":false,\"tools\":[\"read_file\",\"run_code\"]}",
        )
        .unwrap();
        let names = cfg.tools.clone();
        // No unwrap_err: `Vec<Box<dyn Tool>>` isn't Debug. Assert on the
        // error text via match so the feature-ON tree also typechecks this
        // test's body out (it's cfg(not(feature = "code")) gated anyway).
        let err = match build(&names, &cfg, &reqwest::blocking::Client::new(), "k", &[]) {
            Err(e) => format!("{e:#}"),
            Ok(_) => panic!("run_code must not resolve in a feature-off build"),
        };
        assert!(err.contains("run_code") && err.contains("code"), "{err}");
    }

    #[test]
    fn truncate_marks_omission() {
        let mut s = "x".repeat(5000);
        truncate(&mut s, 100);
        assert!(s.chars().count() < 200);
        assert!(s.contains("[truncated, limit 100 chars]"));
    }

    /// A unique, canonicalized temp workdir for confine/resolve tests. confine
    /// compares against a canonicalized workdir (config load canonicalizes it),
    /// so the test must too — on macOS /tmp is a symlink to /private/tmp, and a
    /// non-canonical workdir would make every starts_with check falsely fail.
    fn workdir(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ta-confine-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    fn resolve_rejects_dotdot_escape_when_restricted() {
        let ctx = ToolCtx {
            workdir: workdir("escape"),
            max_output_chars: 1000,
            restrict_to_workdir: true,
            cancel: None,
            shell_timeout_secs: 120,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
        };
        let err = resolve(&ctx, "../outside.txt").unwrap_err();
        assert!(
            err.to_string().contains("escapes the working directory"),
            "{err}"
        );
    }

    #[test]
    fn resolve_allows_a_nested_not_yet_existing_path() {
        let dir = workdir("inside");
        let ctx = ToolCtx {
            workdir: dir.clone(),
            max_output_chars: 1000,
            restrict_to_workdir: true,
            cancel: None,
            shell_timeout_secs: 120,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
        };
        // write_file targets not-yet-existing files; resolve (via confine) must
        // allow a path whose deeper components don't exist yet (it canonicalizes
        // the deepest existing ancestor — the workdir — and re-appends the rest).
        let resolved = resolve(&ctx, "a/b/c.txt").unwrap();
        assert!(resolved.starts_with(&dir));
        assert!(resolved.ends_with("a/b/c.txt"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_skips_confinement_when_unrestricted() {
        let ctx = ToolCtx {
            workdir: std::env::temp_dir(),
            max_output_chars: 1000,
            restrict_to_workdir: false,
            cancel: None,
            shell_timeout_secs: 120,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
        };
        // With restrict off, resolve just joins — no escape check, no error.
        let resolved = resolve(&ctx, "../outside.txt").unwrap();
        assert!(resolved.ends_with("../outside.txt"));
    }

    #[test]
    fn confine_rejects_an_existing_path_outside_workdir() {
        let dir = workdir("abs");
        let ctx = ToolCtx {
            workdir: dir.clone(),
            max_output_chars: 1000,
            restrict_to_workdir: true,
            cancel: None,
            shell_timeout_secs: 120,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
        };
        // The workdir's own parent exists and is, by definition, not under it.
        let outside = dir.parent().unwrap();
        let err = confine(&ctx, outside).unwrap_err();
        assert!(
            err.to_string().contains("escapes the working directory"),
            "{err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
