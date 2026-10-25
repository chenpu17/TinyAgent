//! TinyAgent kernel — the agent loop, tools, providers, and assembly,
//! exposed as the `tiny_agent` library.
//!
//! Third-party Rust code — and the Python / TypeScript bindings built on top
//! of it — can drive the agent loop programmatically: construct an [`Agent`],
//! stream [`AgentEvent`]s, register custom tools, and read back honest token
//! usage. The `tiny-agent` binary (workspace root package) is a process-level
//! shell on top of this crate: CLI argument parsing, SIGINT handling,
//! `process::exit`, headless rendering, batch orchestration, and the optional
//! TUI live there, not here.
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

#![warn(missing_docs)]

/// The agent loop: history, tool dispatch, compression, billing, and the
/// [`Agent`] itself. [`Agent::run`] is the single entry point; everything it
/// returns ([`RunOutcome`] / [`RunError`]) carries honest cost even on failure.
pub mod agent;
pub mod compress;
/// Static configuration ([`Config`]) plus the batch-overlay fragments
/// (`PartialConfig` family). Deserialized with `deny_unknown_fields` — a typo
/// is an error, not a silently ignored key.
pub mod config;
pub mod context_report;
pub mod diag;
pub mod fsutil;
pub mod hooks;
pub mod mcp;
/// Provider wire protocols: [`Provider`] trait, the shared message/block model
/// ([`Block`] / [`Message`] / [`Usage`]), and the Anthropic / OpenAI
/// implementations. `provider::build` picks one from a [`Config`].
pub mod provider;
pub mod retry;
/// Session-file persistence: save / load the conversation (plus the archive of
/// messages folded out by compression) so `--resume` can continue a run.
pub mod session;
pub mod skills;
/// The tool layer: the [`Tool`] trait, [`ToolCtx`], and the built-in
/// implementations (further submodules are internal except `mcp` / `run_code`).
pub mod tools;
// windows, not not(unix): the module is windows-sys all the way down (Job
// Objects), and windows-sys is a cfg(windows) dependency — a third platform
// (wasm32 &c.) pulled in by not(unix) would fail on the missing crate.
#[cfg(windows)]
pub(crate) mod winjob;

/// M0 mechanism probes (`docs/code_llm_design.md` §7「前置（M0，评测与拓扑的
/// 地基）」): in-process prototypes for the reserve/settle ledger, the
/// shared-provider cell, and per-request model routing — the parts of
/// A-L2b/2c/9 provable before any M1–M3 feature code exists.
#[cfg(test)]
#[path = "m0_probes.rs"]
mod m0_probes;

/// Per-worker usage side channel for the parallel tool batch (recovers the
/// spend that used to die with a panicking worker thread).
mod usage_probe;

// ── Facade re-exports (the library's public face) ──────────────────────────
pub use agent::{
    extract_plan, Agent, AgentEvent, CompressError, CompressOutcome, CompressTrigger,
    CompressionRecord, RunCost, RunError, RunOutcome, SupervisorHandle, TodoItem, ToolCallRecord,
    TurnTiming, SUPERVISOR_NOTE_KIND,
};
pub use config::{CodeConfig, Config, Limits, McpServerConfig};
pub use context_report::{
    Calibration, ContextBreakdown, MessageKinds, MessagesBreakdown, SystemBreakdown, ToolCategory,
    ToolEntry, ToolsBreakdown,
};
pub use hooks::{Hooks, ToolDecision};
pub use provider::{
    Block, ChatRequest, ChatResponse, Message, Provider, Role, SimpleChatResponse, ToolDef, Usage,
};
pub use tools::{
    load_document_block, BudgetGate, CallbackTool, Tool, ToolConcurrency, ToolCtx, ToolOrigin,
    ToolOutput,
};
// The sandboxed JS tool only exists behind `--features code`; its CONFIG is
// plain data and unconditional (above) so configs parse on any build.
#[cfg(feature = "code")]
pub use tools::run_code::RunCodeTool;

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
    // The pre-assembly system-prompt length, for context_breakdown's base /
    // skill-intro attribution. Captured BEFORE `assemble` — its returned cfg
    // clone shadows this one and carries the appended skill intro.
    let skill_base = cfg.system_prompt.as_ref().map(|s| s.len()).unwrap_or(0);
    let (tool_list, ctx, cfg, code, eager) = assemble(cfg, client, extra_tools, api_key)?;
    // FR-1 eager skill injection (FD-3 + e2e 修订)：内容判定——已在历史中
    // 披露过（eager 会话 resume / 真实 read_skill 调用）则不重注；headless
    // 与 batch 的任务消息在 initial 里，前置后边界按 append_user_turn 合并。
    let initial = prepend_eager_pair(eager, initial);
    let mut agent = Agent::try_new(
        &cfg,
        provider::build(&cfg, api_key, client.clone()),
        tool_list,
        initial,
        ctx,
    )?;
    // Narrow the attribution to the user's own prompt (assembly window —
    // between try_new and return, no concurrency).
    agent.set_skill_intro_base(skill_base);
    code.install(&mut agent);
    Ok(agent)
}

/// Like [`build_agent_with`], but the caller supplies the [`Provider`] directly
/// instead of it being built from the config. The Python / TypeScript bindings
/// use this to inject a user-supplied custom provider (host-language function
/// wrapped in a `Provider` impl) — and Rust callers use it the same way: it is
/// the public injection point for any **provider-side preprocessing** (e.g.
/// routing PDF document blocks to an endpoint's own document API before
/// sending), reusing the config's tool / MCP assembly unchanged.
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
    // Same pre-assembly capture as `build_agent_with` (its cfg gets shadowed
    // by assemble's intro-augmented clone).
    let skill_base = cfg.system_prompt.as_ref().map(|s| s.len()).unwrap_or(0);
    let (tool_list, ctx, cfg, code, eager) = assemble(cfg, client, extra_tools, api_key)?;
    // Same FR-1 rule as build_agent_with (FD-3 + e2e 修订).
    let initial = prepend_eager_pair(eager, initial);
    let mut agent = Agent::try_new(&cfg, provider, tool_list, initial, ctx)?;
    agent.set_skill_intro_base(skill_base);
    code.install(&mut agent);
    Ok(agent)
}

/// What `assemble` hands back for the `code` feature: the ONE top-level
/// `run_code` instance (design §8.6, H3 single construction point). The type
/// exists unconditionally — feature-off builds get the default (empty) one —
/// so `assemble`'s signature doesn't fork per feature.
#[derive(Default)]
struct CodeInstall {
    #[cfg(feature = "code")]
    tool: Option<tools::run_code::RunCodeTool>,
    /// Assembly-time audit facts (finetune_support NFR-3 + LD-12) — `Some`
    /// only when the sandbox is enabled.
    #[cfg(feature = "code")]
    audit: Option<crate::agent::CodeAudit>,
}

impl CodeInstall {
    /// Hand the instance to a freshly built agent. No-op without the feature.
    fn install(self, agent: &mut Agent) {
        #[cfg(feature = "code")]
        {
            agent.set_code_audit(self.audit);
            if let Some(t) = self.tool {
                agent.set_code_tool(t);
            }
        }
        #[cfg(not(feature = "code"))]
        let _ = agent;
    }
}

/// Shared tool-list + [`ToolCtx`] assembly for the `build_agent*` helpers:
/// resolve `workdir`, build the config's MCP tools once, wire the config's
/// built-in tool whitelist (passing the shared MCP set so a `task` sub-agent
/// reuses the parent's connections rather than re-spawning), then append MCP +
/// any extra tools. Factored out so [`build_agent_with`] and
/// [`build_agent_with_provider`] stay in lockstep — only the provider
/// construction differs between them.
#[allow(clippy::type_complexity)] // returns (tools, ctx, cfg, code, eager): cfg carries the skill-augmented system_prompt; eager = FR-1 synthetic skill pair (code feature, else None)
fn assemble(
    cfg: &Config,
    client: &reqwest::blocking::Client,
    extra_tools: Vec<Box<dyn tools::Tool>>,
    api_key: &str,
) -> anyhow::Result<(
    Vec<Box<dyn tools::Tool>>,
    ToolCtx,
    Config,
    CodeInstall,
    Option<Vec<Message>>,
)> {
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

    // run_code (H3/H4): the tool's membership in the model-visible list is
    // driven by `code.enabled`, NOT by the whitelist — so strip the name here
    // and let the single constructed instance be the ONLY top-level run_code.
    // With the feature off the name falls through to tools::build's unknown
    // arm, whose message names the remedy (rebuild with --features code).
    // The retain below only exists with the code feature; without it the
    // binding is never mutated.
    #[cfg_attr(not(feature = "code"), allow(unused_mut))]
    let mut whitelist = cfg.tools.clone();
    #[cfg(feature = "code")]
    {
        if whitelist.iter().any(|t| t == "run_code") && !cfg.code.enabled {
            anyhow::bail!(
                "run_code listed in tools but code.enabled=false — set \
                 code.enabled=true to enable it, or drop run_code from tools"
            );
        }
        whitelist.retain(|t| t != "run_code");
    }
    #[cfg(not(feature = "code"))]
    if cfg.code.enabled {
        // The config parses on any build (CodeConfig is plain data), so an
        // enabled flag on a feature-less build would otherwise be a silent
        // no-op (sec-review n-3) — say so once per PROCESS: batch assembles
        // one agent per task and would otherwise print this N times
        // (correctness-review n-2).
        static SAID: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        SAID.get_or_init(|| {
            crate::diag::diag_warn!(
                "[config] code.enabled=true ignored: this build lacks the `code` \
                 cargo feature — rebuild with --features code to get run_code"
            );
        });
    }
    let mut tool_list = tools::build(&whitelist, cfg, client, api_key, &mcp_tools)?;
    // Borrow, don't consume: the run_code snapshot below still needs the set.
    tool_list.extend(
        mcp_tools
            .iter()
            .cloned()
            .map(|t| Box::new(t) as Box<dyn tools::Tool>),
    );
    // Host-injected tools: Arc-shared so the SAME instance serves the
    // top-level list (via SharedTool) and — only if the tool opts in with
    // `available_to_code()` — the run_code guest registry (v2 review: the
    // registry previously rebuilt from the whitelist alone, so an injected
    // tool was "usable top-level, invisible in code").
    let extra_shared: Vec<std::sync::Arc<Box<dyn tools::Tool>>> =
        extra_tools.into_iter().map(std::sync::Arc::new).collect();
    tool_list.extend(extra_shared.iter().map(|t| {
        Box::new(tools::SharedTool {
            inner: std::sync::Arc::clone(t),
        }) as Box<dyn tools::Tool>
    }));

    // The single run_code instance (code feature). Constructed here because
    // only assemble holds all four snapshot inputs; cheap (field clones — the
    // engine is a process-level OnceLock, the registry builds lazily on first
    // use). Enabled (D15, default false) → a clone goes into the tool list so
    // the model sees it; the live handle travels back via CodeInstall so the
    // agent's switch methods (set_code_config / set_code_enabled) can reach
    // the same shared config Arc as the boxed instance (A-10).
    #[cfg(feature = "code")]
    let code = {
        let tool = tools::run_code::RunCodeTool::new(
            cfg,
            std::sync::Arc::new(std::sync::RwLock::new(cfg.code.clone())),
            client,
            api_key,
            mcp_tools.to_vec(),
            extra_shared.clone(),
        );
        if cfg.code.enabled {
            tool_list.push(Box::new(tool.clone()));
        }
        CodeInstall {
            tool: Some(tool),
            // Captured at assembly (FD-7): skill_injection is an
            // assembly-time decision — a later set_code_config replace must
            // not rewrite the audit of what actually happened. The llm
            // anchor (LD-12) rides the same snapshot rule.
            audit: cfg.code.enabled.then(|| crate::agent::CodeAudit {
                skill_injection: cfg.code.skill_injection,
                precheck: cfg.code.precheck,
                llm: cfg.code.llm.enabled.then(|| cfg.code.llm.clone()),
            }),
        }
    };
    #[cfg(not(feature = "code"))]
    let code = CodeInstall::default();

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
    // `mut` only for the code-feature built-in injection below (same
    // cfg_attr pattern as `whitelist` above).
    #[cfg_attr(not(feature = "code"), allow(unused_mut))]
    let (mut index, warnings) = skills::scan(&skill_dirs);
    for w in &warnings {
        crate::diag::diag_warn!("[skill] {w}");
    }
    // Built-in skill (v2 P2): run_code's full usage guide ships IN the binary
    // as an embedded skill — the tool description stays slim (closed helper
    // list + a pointer) while the deep contracts load on demand via
    // read_skill as compressible history. `insert_builtin` is a no-op when a
    // disk skill of the same name exists, so a user's run-code/ override
    // always wins. A malformed built-in (build bug) degrades to a warning,
    // never a startup failure — same contract as a bad disk skill.
    //
    // `code.skill_injection` (finetune_support_design FR-1) branches this:
    //   on_demand — status quo above (byte-identical default);
    //   eager     — also inject the body into history as a synthetic
    //               read_skill tool_use/tool_result pair (FD-3/FD-4);
    //   off       — skip the builtin entirely; tombstone read_skill when no
    //               disk override serves the name (FD-5).
    #[cfg(feature = "code")]
    let (mut eager_pair, mut run_code_tombstone) = (None, false);
    #[cfg(not(feature = "code"))]
    let eager_pair: Option<Vec<Message>> = None;
    #[cfg(feature = "code")]
    if cfg.code.enabled {
        use crate::config::SkillInjection;
        match cfg.code.skill_injection {
            SkillInjection::Off => {
                run_code_tombstone = index.entry_of(RUN_CODE_SKILL_NAME).is_none();
            }
            policy @ (SkillInjection::OnDemand | SkillInjection::Eager) => {
                // MD-13 / LD-15 / LD-14: the lib, batch and llm sections
                // ride their assembly-time gates — spliced out when off, so
                // a gate-off skill stays the pre-gate contract.
                match skills::embedded_entry(&tools::run_code::run_code_skill_md(
                    cfg.code.include,
                    cfg.code.call_batch,
                    cfg.code.llm.enabled,
                )) {
                    Some(e) => index.insert_builtin(e),
                    None => crate::diag::diag_warn!(
                        "[skill] built-in run-code skill is malformed, skipped"
                    ),
                }
                if policy == SkillInjection::Eager {
                    // Serve the EFFECTIVE entry (a disk override wins) through
                    // the same function read_skill uses — byte-identity by
                    // construction (FD-4). Failure degrades to a warning and
                    // no injection (the on_demand path still works).
                    match index.entry_of(RUN_CODE_SKILL_NAME) {
                        Some(entry) => {
                            match tools::serve_skill_body(entry, cfg.limits.max_output_chars) {
                                Ok(out) if !out.is_error => {
                                    eager_pair = Some(eager_read_skill_pair(&out.content))
                                }
                                Ok(out) => {
                                    crate::diag::diag_warn!("[skill] eager injection skipped: {}", out.content)
                                }
                                Err(e) => crate::diag::diag_warn!("[skill] eager injection skipped: {e:#}"),
                            }
                        }
                        None => crate::diag::diag_warn!(
                            "[skill] eager injection skipped: {RUN_CODE_SKILL_NAME} skill unavailable"
                        ),
                    }
                }
            }
        }
    }
    // allowed-tools (experimental frontmatter field): warn-only — TinyAgent has
    // no per-turn permission/activation layer, so it can't grant a skill's
    // declared tools for a single turn the way Claude Code does. Surface a
    // declared tool that isn't available so the author notices the mismatch;
    // literal-name match only (the spec is space-separated tool names; Claude
    // Code's glob/`Tool(spec)` patterns are an extension we don't support).
    // `run_code` is known when enabled — it's driven by code.enabled rather
    // than the whitelist the rest of the check reads (see the strip above).
    for e in index.entries() {
        for t in &e.allowed_tools {
            let known = cfg.tools.iter().any(|c| c == t)
                || t == "read_skill"
                || t == "list_skill"
                || (t == "run_code" && cfg.code.enabled);
            if !known {
                crate::diag::diag_warn!(
                    "[skill] {} declares allowed-tool {:?} not in the tool whitelist",
                    e.name,
                    t
                );
            }
        }
    }
    let mut cfg = cfg.clone();
    if !index.is_empty() {
        cfg.system_prompt = Some(skills::render_intro(cfg.system_prompt.as_deref(), &index));
        #[cfg_attr(not(feature = "code"), allow(unused_mut))]
        let mut read_skill = tools::ReadSkill::new(index.clone());
        // FR-1 off: explicit "disabled by configuration" error instead of the
        // generic unknown-skill one (FD-5) — only reachable when other skills
        // keep read_skill itself registered.
        #[cfg(feature = "code")]
        if run_code_tombstone {
            read_skill.disable_builtin(RUN_CODE_SKILL_NAME);
        }
        tool_list.push(Box::new(read_skill) as Box<dyn tools::Tool>);
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
        tool_absent_notes: cfg.tool_absent_notes.clone(),
    };
    Ok((tool_list, ctx, cfg, code, eager_pair))
}

/// Prepend the eager skill pair to a caller-supplied `initial` history
/// (FD-3, e2e 修订：判定从「initial 为空」改为内容判定)。The original
/// empty-only criterion never fires on the HEADLESS paths — `main.rs` and
/// `batch.rs` both put the task message into `initial`, so a fresh bench run
/// always looked "resumed" and the pair was silently skipped.
///
/// Injection is skipped when the history already carries the disclosure:
/// a resumed eager session contains the pair itself, and a history with a
/// real `read_skill("run-code")` exchange already has the body — either way
/// re-injecting duplicates it (the resume rule's actual intent).
///
/// Boundary: the pair ends in a `user(tool_result)` message, and a leading
/// user task message must MERGE into it (`append_user_turn`'s rule — two
/// adjacent user messages 400 on Anthropic). The merged shape
/// (`[tool_result, text]` in one user message) is exactly what the bindings /
/// TUI paths produce today: they build on an empty `initial` and the first
/// `run()` merges the task the same way.
fn prepend_eager_pair(eager: Option<Vec<Message>>, initial: Vec<Message>) -> Vec<Message> {
    let Some(mut pair) = eager else {
        return initial;
    };
    let already_disclosed = initial.iter().any(|m| {
        m.role == Role::Assistant
            && m.content.iter().any(|b| {
                matches!(
                    b,
                    Block::ToolUse { name, input, .. }
                        if name == "read_skill"
                            && input.get("skill").and_then(serde_json::Value::as_str)
                                == Some("run-code")
                )
            })
    });
    if already_disclosed {
        return initial;
    }
    let mut rest = initial;
    if rest.first().is_some_and(|m| m.role == Role::User) {
        let first = rest.remove(0);
        match pair.last_mut() {
            Some(last_user) => last_user.content.extend(first.content),
            None => rest.insert(0, first),
        }
    }
    pair.extend(rest);
    pair
}

/// Name of the built-in run_code skill (finetune_support_design FR-1) — the
/// frontmatter `name:` of `tools/run_code_skill.md`. A literal here (not a
/// parse) because the off/eager branches must name the entry BEFORE/AROUND
/// `insert_builtin`; a test pins it against the parsed frontmatter.
#[cfg(feature = "code")]
const RUN_CODE_SKILL_NAME: &str = "run-code";

/// Fixed tool_use id of the eager injection's synthetic pair (FD-3). The real
/// path's id is provider-minted randomness, so byte-identity covers the
/// message SHAPE (roles, block kinds, content) — a stable literal here makes
/// the synthetic pair mechanically recognizable in bench trajectories.
#[cfg(feature = "code")]
const EAGER_TOOL_USE_ID: &str = "eager-run-code";

/// The synthetic read_skill round-trip injected for `skill_injection =
/// "eager"` (FD-3/FD-4): assistant tool_use + user tool_result, N:N paired,
/// byte-identical in shape to a real `read_skill("run-code")` exchange
/// (agent.rs's assistant/tool_result pushes). The pair's tool_use id IS set
/// — the fixed [`EAGER_TOOL_USE_ID`] on both halves, making the synthetic
/// exchange mechanically recognizable — while message-level ids stay unset:
/// the pair enters via `initial`, and `Agent::try_new`'s ingest point
/// (`assign_missing_ids`) fills only MISSING message ids (existing ones
/// pass through untouched), the same as any fresh history.
#[cfg(feature = "code")]
fn eager_read_skill_pair(body: &str) -> Vec<Message> {
    vec![
        Message::assistant(vec![Block::ToolUse {
            id: EAGER_TOOL_USE_ID.into(),
            name: "read_skill".into(),
            input: serde_json::json!({"skill": RUN_CODE_SKILL_NAME}),
        }]),
        Message::user(vec![Block::ToolResult {
            tool_use_id: EAGER_TOOL_USE_ID.into(),
            content: body.to_string(),
            image: None,
            is_error: false,
        }]),
    ]
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
