//! The `task` tool: a sequential sub-agent.
//!
//! The parent agent delegates a scoped piece of work to a fresh child agent
//! (its own history + turn budget), then receives only the child's final
//! summary as the tool result — the parent's context stays uncluttered by the
//! child's scratch work. The child's token usage is folded into the parent's
//! totals (honest cost) via `ToolOutput::usage`.
//!
//! Scope:
//! - **Parallel within a turn.** Multiple `task` calls in one assistant turn
//!   run concurrently (the agent loop fans them out on scoped threads); a
//!   single `task` call just blocks its turn until the child finishes. No
//!   workdir isolation — children share the parent's workdir (concurrent
//!   sub-agents are expected not to write the same file).
//! - **No recursion.** The child's tool list is the parent's whitelist retained
//!   by `Tool::available_to_subagent` — `task` opts out, so a sub-agent cannot
//!   spawn its own sub-agent. This sidesteps depth bookkeeping entirely.
//! - **Event forwarding.** The child's events stream into the parent's sink,
//!   each wrapped as `AgentEvent::SubAgent { name, .. }` so a harness/UI can
//!   show each sub-agent's progress and tell parallel ones apart.
//! - **Shared workdir + cancel.** The child inherits the parent's `workdir`,
//!   `restrict_to_workdir`, and cancel flag (so Ctrl-C interrupts the child
//!   mid-stream, not only at the next parent turn). The child runs via
//!   `Agent::run_as_subagent`, which — unlike the top-level `run` — does NOT
//!   reset the shared flag at entry. That reset would race a sibling's Ctrl-C
//!   in a parallel batch (a later-starting child clearing a flag another child
//!   is mid-responding to); skipping it is safe because the batch gate ensures
//!   the flag is false at spawn, so any `true` seen mid-run is a real cancel.

use crate::agent::{Agent, AgentEvent, RunOutcome, ToolCallRecord};
use crate::config::Config;
use crate::provider::{self, Block, Message, Usage};
use crate::tools::{self, Tool, ToolConcurrency, ToolCtx, ToolOutput};
use anyhow::Result;
use serde_json::{json, Value};

/// Focused default persona for a sub-agent. The parent hands it a concrete
/// prompt; the child just needs to do the work and return a tight summary.
const SUBAGENT_SYSTEM_PROMPT: &str = "\
You are a focused sub-agent. Complete the assigned task using the available \
tools, verify your work, then return a concise result summary. Do not ask \
questions — make a reasonable decision and proceed.";

/// Cap on a sub-agent's turn budget, regardless of the parent's `max_turns`,
/// so a stuck sub-agent can't pin the parent forever. The parent's run is the
/// real wall-clock guard (shared cancel flag); this bounds the LLM loop.
const SUBAGENT_MAX_TURNS: u32 = 25;

/// The `task` sub-agent tool. Holds what it needs to build a fresh child agent
/// (provider config + HTTP client + key), constructed once at `tools::build`.
/// `mcp_tools` are the parent's SHARED MCP tools (Arc-backed connections): the
/// child clones them rather than re-running `mcp::build`, so a `task` call no
/// longer re-spawns every MCP server.
pub struct SubAgent {
    cfg: Config,
    client: reqwest::blocking::Client,
    api_key: String,
    mcp_tools: Vec<crate::tools::mcp::McpTool>,
}

impl SubAgent {
    pub fn new(
        cfg: Config,
        client: reqwest::blocking::Client,
        api_key: String,
        mcp_tools: Vec<crate::tools::mcp::McpTool>,
    ) -> Self {
        Self {
            cfg,
            client,
            api_key,
            mcp_tools,
        }
    }
}

impl Tool for SubAgent {
    fn def(&self) -> crate::provider::ToolDef {
        crate::provider::ToolDef {
            name: "task".into(),
            description: "Delegate a scoped piece of work to a sub-agent with its own context. \
                Give a concrete, self-contained prompt; the sub-agent runs to completion and \
                returns only its final summary. Use for focused subtasks (e.g. investigate a \
                file, implement one function) to keep the main context clean."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "A short identifier for this sub-agent (its events are tagged with it)."
                    },
                    "description": {
                        "type": "string",
                        "description": "A short (3-5 word) label for what this task does."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The full, self-contained instruction for the sub-agent."
                    }
                },
                "required": ["name", "prompt"]
            }),
        }
    }

    fn concurrency(&self) -> ToolConcurrency {
        // Each `task` call is an independent child agent (own history, own
        // turn budget) with no shared mutable state — so multiple calls in one
        // turn may fan out on scoped threads. This marker is what routes the
        // call into the parallel batch — nothing in the loop matches the name
        // "task" anymore.
        ToolConcurrency::Parallel
    }

    fn available_to_subagent(&self) -> bool {
        // No recursion: a sub-agent cannot spawn its own sub-agent (sidesteps
        // depth bookkeeping entirely).
        false
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        // The agent loop always calls `run_with_emit` (so child events stream
        // into the parent). This `run` exists only to satisfy the trait; it
        // runs the sub-agent the same way, just with a discarded event stream.
        self.run_with_emit(input, ctx, &mut |_| {})
    }

    fn run_with_emit(
        &self,
        input: &Value,
        ctx: &ToolCtx,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<ToolOutput> {
        let name = input["name"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("task requires a non-empty `name`"))?;
        let prompt = input["prompt"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("task requires a non-empty `prompt`"))?;
        // `description` is accepted but informational; `name` is the identity
        // that tags this sub-agent's forwarded events.

        // Child config: sub-agent persona + a capped turn budget.
        let mut child_cfg = self.cfg.clone();
        child_cfg.system_prompt = Some(SUBAGENT_SYSTEM_PROMPT.into());
        child_cfg.limits.max_turns = child_cfg.limits.max_turns.min(SUBAGENT_MAX_TURNS);

        // Child tools: the parent's whitelist built full, then RETAINED by
        // capability (`available_to_subagent`) rather than filtered by name —
        // `task` and `todo` opt out via the trait (no recursion; plan tracking
        // is the parent's job), and a third-party tool can do the same. MCP
        // servers are inherited (a sub-agent may need the same external tools)
        // by CLONING the parent's shared MCP tools — each clone Arc-shares the
        // underlying connection, so this does NOT re-spawn the MCP servers
        // (the old path called tools::mcp::build, running McpClient::start for
        // every server on every task call — a parallel task batch would spawn
        // N×(#servers) subprocesses).
        let mut child_tools = child_tools(&child_cfg, &self.client, &self.api_key, &self.mcp_tools)?;
        child_tools.extend(self.mcp_tools.iter().cloned().map(|t| Box::new(t) as Box<dyn Tool>));

        let provider = provider::build(&child_cfg, &self.api_key, self.client.clone());
        let initial = vec![Message::user(vec![Block::Text(prompt.to_string())])];
        let mut child = Agent::new(&child_cfg, provider, child_tools, initial, child_ctx(ctx, name));
        if let Some(flag) = ctx.cancel.clone() {
            child.set_cancel(flag);
        }
        Ok(run_subagent(&mut child, name, emit, ctx.max_output_chars))
    }
}

/// The sub-agent's whitelist tools: build the config's FULL tool list, then
/// retain only the tools whose [`Tool::available_to_subagent`] says true.
/// Capability-based, not name-based — `task` (no recursion) and `todo` (the
/// parent owns the plan) opt out via the trait, and so can any third-party
/// tool. The caller appends the shared MCP tools separately (they're injected
/// at assembly, never part of the whitelist).
fn child_tools(
    cfg: &Config,
    client: &reqwest::blocking::Client,
    api_key: &str,
    mcp_tools: &[crate::tools::mcp::McpTool],
) -> Result<Vec<Box<dyn Tool>>> {
    let mut out = tools::build(&cfg.tools, cfg, client, api_key, mcp_tools)?;
    out.retain(|t| t.available_to_subagent());
    Ok(out)
}

/// The sub-agent's execution context: inherits the parent's sandbox / output
/// limit / cancel flag / shell timeout, and additionally (commit 4)
/// - INHERITS the hooks layer by Arc clone — a tool denied at the top level
///   stays denied inside a sub-agent (no bypass), and the child's own
///   `before_tool`/`after_tool`/`on_chat_request` flow through the same
///   layer (on the child's worker thread — see `Hooks` doc, "Concurrency");
/// - TAGS the sub-agent identity (`subagent`) so a hooks impl can tell a
///   sub-agent's calls from top-level ones.
fn child_ctx(ctx: &ToolCtx, name: &str) -> ToolCtx {
    ToolCtx {
        workdir: ctx.workdir.clone(),
        max_output_chars: ctx.max_output_chars,
        restrict_to_workdir: ctx.restrict_to_workdir,
        cancel: ctx.cancel.clone(),
        shell_timeout_secs: ctx.shell_timeout_secs,
        hooks: ctx.hooks.clone(),
        subagent: Some(name.to_string()),
    }
}

/// Drive a constructed child agent to completion and map its outcome to a tool
/// result. Split out from `run_with_emit` so it's unit-testable with an
/// in-process `MockProvider` (no HTTP): build any `Agent`, hand it here, assert
/// the `ToolOutput`. The child's events are forwarded to `emit`, each wrapped as
/// `AgentEvent::SubAgent { name, .. }` so the parent stream shows the
/// sub-agent's progress tagged with its identity.
fn run_subagent(
    agent: &mut Agent,
    name: &str,
    emit: &mut dyn FnMut(AgentEvent),
    max_output_chars: usize,
) -> ToolOutput {
    let name_owned = name.to_string();
    // run_as_subagent (not run): the child shares the parent's cancel Arc, and
    // must NOT reset it at entry — otherwise a sibling in a parallel `task`
    // batch starting later would clear a Ctrl-C just signalled on the shared
    // flag. See Agent::run_as_subagent.
    match agent.run_as_subagent(&mut |ev| {
        emit(AgentEvent::SubAgent {
            name: name_owned.clone(),
            event: Box::new(ev),
        });
    }) {
        Ok(o) => outcome_to_output(&o, name, max_output_chars),
        Err(re) => {
            let mut out = ToolOutput::error(format!("subagent {name} failed: {re:#}"));
            // Honest cost even on failure: the turns the child completed before
            // erroring were real billed calls — dropping them would understate
            // the parent run's totals. The RunError carries exactly the partial
            // usage burned (None-equivalent when nothing was billed yet: attaching
            // a zero usage would be a noise event, so gate on total > 0). `total`
            // covers cache read/creation too — a child whose only cost was a
            // prompt-cache hit (input/output zero) must still be billed.
            if re.usage.total() > 0 {
                out.usage = Some(re.usage);
            }
            // Audit trace even on failure: "how far did the child get" is the
            // first question when a parallel `task` fails, and it lives only in
            // NDJSON otherwise. stop_reason is "error" here (the child's own
            // stop_reason was never set — it returned Err), so the trace makes
            // that explicit rather than echoing the parent-facing failure text.
            out.meta = Some(trace_meta(re.turns, &re.tool_calls, &re.usage, "error"));
            out
        }
    }
}

/// Turn a completed child run into the parent-facing tool result: the child's
/// final summary as content, its usage billed to the parent, and a non-alarming
/// note when it ended short of `end_turn` (max_turns / timeout / cancelled),
/// tagged with the sub-agent's name so the parent can tell which one stopped.
fn outcome_to_output(o: &RunOutcome, name: &str, max_output_chars: usize) -> ToolOutput {
    let usage = Usage {
        input_tokens: o.input_tokens,
        output_tokens: o.output_tokens,
        cache_creation_input_tokens: o.cache_creation_input_tokens,
        cache_read_input_tokens: o.cache_read_input_tokens,
    };
    let mut content = if o.stop_reason == "end_turn" {
        o.final_text.clone()
    } else {
        format!("[subagent {name} ended: {}]\n{}", o.stop_reason, o.final_text)
    };
    // Cap the summary to the parent's output budget: a child that ignores the
    // "concise summary" instruction and dumps its whole transcript would
    // inject a huge tool result into the parent's context, bloating it toward
    // the very compression the sub-agent was meant to avoid. Char-boundary safe.
    tools::truncate(&mut content, max_output_chars);
    // Compact audit trace into `meta` (NOT content): keeps the parent's context
    // clean (README: "only the final summary as the tool result") while making
    // `result.json` self-contained for auditing parallel work — turns, which
    // tools the child invoked and how often, a rough token cost, and the stop
    // reason. Lands in `tool_calls[].meta` for the `task` entry only.
    let meta = trace_meta(o.turns, &o.tool_calls, &usage, &o.stop_reason);
    ToolOutput {
        content,
        image: None,
        // A short stop isn't necessarily a failure (partial work may be useful),
        // so don't flag is_error — the prefix note above tells the parent what
        // happened. Only a provider Err (handled in run_subagent) is_error=true.
        is_error: false,
        usage: Some(usage),
        meta: Some(meta),
    }
}

/// Build the compact per-subagent audit trace attached as `ToolOutput::meta`.
/// `tools` counts child tool calls by name (order-preserved map); `tokens.in`
/// sums every input-facing axis (uncached input + cache write + cache read) so
/// one number reflects how much the child read, while the exact cache split is
/// already folded into the parent run's top-level totals.
fn trace_meta(turns: u32, tool_calls: &[ToolCallRecord], usage: &Usage, stop_reason: &str) -> Value {
    let mut tools = serde_json::Map::new();
    for c in tool_calls {
        if let Some(n) = tools.get_mut(&c.name) {
            *n = json!(n.as_u64().unwrap_or(0) + 1);
        } else {
            tools.insert(c.name.clone(), json!(1u64));
        }
    }
    json!({
        "turns": turns,
        "tools": tools,
        "tokens": {
            "in": usage.input_tokens
                + usage.cache_creation_input_tokens
                + usage.cache_read_input_tokens,
            "out": usage.output_tokens,
        },
        "stop_reason": stop_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::mock::{self, MockProvider};
    use serde_json::json;

    /// Build a child agent backed by a scripted MockProvider (no network), with
    /// a no-cancel ToolCtx. Mirrors what SubAgent::run assembles, minus the
    /// real provider — so we can assert run_subagent's outcome mapping.
    fn mock_agent(turns: Vec<mock::MockTurn>) -> Agent {
        let cfg = crate::config::Config {
            provider: crate::config::ProviderKind::Anthropic,
            base_url: None,
            api_key_env: "X".into(),
            model: "m".into(),
            max_tokens: 64,
            system_prompt: None,
            task: None,
            workdir: None,
            tools: vec![],
            restrict_to_workdir: false,
            limits: crate::config::Limits::default(),
            mcp_servers: vec![],
            reasoning_effort: None,
            inject_environment: false,
        };
        let (prov, _state) = MockProvider::new(turns);
        Agent::new(
            &cfg,
            Box::new(prov),
            vec![],
            vec![Message::user(vec![Block::Text("do it".into())])],
            ToolCtx {
                workdir: std::env::temp_dir(),
                max_output_chars: 1000,
                restrict_to_workdir: false,
                cancel: None,
                shell_timeout_secs: 120,
                hooks: None,
                subagent: None,
            },
        )
    }

    #[test]
    fn run_subagent_returns_final_text_and_bills_usage() {
        // Two turns so usage AGGREGATES across them: turn 1 is a tool_use (the
        // child has no matching tool → an error result, but the loop continues
        // to turn 2), turn 2 is the final text + end_turn. The parent-facing
        // result must carry the child's last text as summary and the summed
        // usage (100+50 input, 5+3 output) so cost is honest.
        let mut agent = mock_agent(vec![
            mock::resp_tool("t1", "noop", json!({}), mock::mk_usage(100, 5)),
            mock::resp_text("done summary", mock::mk_usage(50, 3)),
        ]);
        let out = run_subagent(&mut agent, "scout", &mut |_| {}, usize::MAX);
        assert_eq!(out.content, "done summary");
        assert!(!out.is_error);
        let u = out.usage.expect("usage billed to parent");
        assert_eq!(u.input_tokens, 150);
        assert_eq!(u.output_tokens, 8);
    }

    #[test]
    fn run_subagent_error_still_bills_completed_turns() {
        // The child completes turn 1 (billed 100/5), then the provider errors
        // on turn 2. The run fails, but turn 1's tokens were really burned —
        // they must reach the parent's totals (honest cost), not vanish.
        let mut agent = mock_agent(vec![
            mock::resp_tool("t1", "noop", json!({}), mock::mk_usage(100, 5)),
            mock::MockTurn::Err("provider blew up".into()),
        ]);
        let out = run_subagent(&mut agent, "scout", &mut |_| {}, usize::MAX);
        assert!(out.is_error);
        assert!(out.content.contains("provider blew up"));
        let u = out.usage.expect("failed sub-agent still bills completed turns");
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 5);
    }

    #[test]
    fn run_subagent_error_with_no_completed_turns_has_no_usage() {
        // Error on the very first request: nothing was billed, so usage must
        // stay None (the parent folds only Some — a Some(0,0) would be noise).
        let mut agent = mock_agent(vec![mock::MockTurn::Err("instant failure".into())]);
        let out = run_subagent(&mut agent, "scout", &mut |_| {}, usize::MAX);
        assert!(out.is_error);
        assert!(out.usage.is_none());
    }

    #[test]
    fn run_subagent_forwards_events_tagged_with_name() {
        // The child's events must reach the parent stream wrapped as
        // SubAgent{name, ..} — that identity is the whole point (parallel
        // sub-agents interleave; the name disambiguates them).
        let mut agent = mock_agent(vec![mock::resp_text("hi", mock::mk_usage(1, 1))]);
        let mut seen: Vec<(String, String)> = Vec::new();
        run_subagent(&mut agent, "worker-7", &mut |ev| {
            if let AgentEvent::SubAgent { name, event } = ev {
                match *event {
                    AgentEvent::TextDelta(t) => seen.push((name.clone(), t)),
                    AgentEvent::TurnStart(n) => seen.push((name.clone(), format!("turn{n}"))),
                    _ => {}
                }
            }
        }, usize::MAX);
        assert!(seen.iter().all(|(n, _)| n == "worker-7"), "every event tagged: {seen:?}");
        assert!(seen.iter().any(|(_, t)| t == "hi"), "child text forwarded: {seen:?}");
    }

    #[test]
    fn outcome_to_output_end_turn_carries_usage() {
        let o = RunOutcome {
            final_text: "all done".into(),
            turns: 2,
            stop_reason: "end_turn".into(),
            tool_calls: vec![],
            input_tokens: 100,
            output_tokens: 5,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            elapsed: std::time::Duration::from_secs_f64(1.0),
        };
        let out = outcome_to_output(&o, "a", usize::MAX);
        assert_eq!(out.content, "all done");
        assert!(!out.is_error);
        let u = out.usage.expect("usage billed");
        assert_eq!((u.input_tokens, u.output_tokens), (100, 5));
    }

    #[test]
    fn outcome_to_output_attaches_trace_meta_not_in_content() {
        // The audit trace (turns / tool counts / token subtotal / stop reason)
        // rides in `meta` — NOT `content` — so the parent's context stays clean
        // (only the summary text reaches the model) while result.json is
        // self-contained. Pins the B design: separation of model view vs audit.
        let o = RunOutcome {
            final_text: "summary".into(),
            turns: 3,
            stop_reason: "end_turn".into(),
            tool_calls: vec![
                ToolCallRecord {
                    name: "shell".into(),
                    input: json!({}),
                    output: "ok".into(),
                    is_error: false,
                    meta: None,
                },
                ToolCallRecord {
                    name: "read_file".into(),
                    input: json!({}),
                    output: "..".into(),
                    is_error: false,
                    meta: None,
                },
                ToolCallRecord {
                    name: "shell".into(),
                    input: json!({}),
                    output: "ok2".into(),
                    is_error: false,
                    meta: None,
                },
            ],
            input_tokens: 1000,
            output_tokens: 40,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 50,
            elapsed: std::time::Duration::from_secs_f64(3.0),
        };
        let out = outcome_to_output(&o, "scout", usize::MAX);
        // content is the clean summary — no trace mixed in.
        assert_eq!(out.content, "summary");
        let meta = out.meta.expect("trace meta attached");
        assert_eq!(meta["turns"], 3);
        assert_eq!(meta["tools"]["shell"], 2, "shell counted twice: {meta}");
        assert_eq!(meta["tools"]["read_file"], 1);
        // tokens.in sums every input-facing axis (input + cache_read here).
        assert_eq!(meta["tokens"]["in"], 1050);
        assert_eq!(meta["tokens"]["out"], 40);
        assert_eq!(meta["stop_reason"], "end_turn");
    }

    #[test]
    fn run_subagent_error_attaches_trace_meta() {
        // Even a failed child has audit value — how far it got. The error path
        // builds the same trace (stop_reason "error") from the RunError.
        let mut agent = mock_agent(vec![
            mock::resp_tool("t1", "noop", json!({}), mock::mk_usage(100, 5)),
            mock::MockTurn::Err("provider blew up".into()),
        ]);
        let out = run_subagent(&mut agent, "scout", &mut |_| {}, usize::MAX);
        assert!(out.is_error);
        let meta = out.meta.expect("trace meta on failure too");
        assert_eq!(meta["stop_reason"], "error");
        assert!(meta["turns"].as_u64().unwrap_or(0) >= 1, "counted the turn that ran");
    }

    #[test]
    fn outcome_to_output_short_stop_notes_reason_not_error() {
        // max_turns / timeout / cancelled: surface the reason as a prefix (with
        // the sub-agent's name), but don't mark is_error — partial work may
        // still be useful to the parent.
        let o = RunOutcome {
            final_text: "partial".into(),
            turns: 25,
            stop_reason: "max_turns_reached".into(),
            tool_calls: vec![],
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            elapsed: std::time::Duration::from_secs_f64(2.0),
        };
        let out = outcome_to_output(&o, "scout", usize::MAX);
        assert!(!out.is_error, "short stop must not be is_error");
        assert!(out.content.starts_with("[subagent scout ended: max_turns_reached]"));
        assert!(out.content.contains("partial"));
    }

    #[test]
    fn outcome_to_output_caps_oversized_summary() {
        // A child that ignores "concise summary" and dumps a huge transcript
        // must not inject it whole into the parent's context — cap to
        // max_output_chars (char-safe, with a marker) so the parent's context
        // stays bounded, the whole point of offloading to a sub-agent.
        let big = "x".repeat(5000);
        let o = RunOutcome {
            final_text: big,
            turns: 1,
            stop_reason: "end_turn".into(),
            tool_calls: vec![],
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            elapsed: std::time::Duration::from_secs_f64(1.0),
        };
        let out = outcome_to_output(&o, "scout", 1000);
        assert!(
            out.content.len() < 2000,
            "summary must be capped, got {} bytes",
            out.content.len()
        );
        assert!(out.content.contains("truncated"), "cap must leave a marker");
        // Only the tail is dropped — the head of the summary survives.
        assert!(out.content.starts_with("xxxx"));
    }

    #[test]
    fn subagent_tool_rejects_missing_or_empty_name_and_prompt() {
        // Validate before building anything: a missing/empty name or prompt is a
        // caller bug, not a sub-agent run to attempt.
        let sa = SubAgent::new(fake_cfg(), reqwest::blocking::Client::new(), "k".into(), vec![]);
        let ctx = ToolCtx {
            workdir: std::env::temp_dir(),
            max_output_chars: 1000,
            restrict_to_workdir: false,
            cancel: None,
            shell_timeout_secs: 120,
            hooks: None,
            subagent: None,
        };
        let mut emit = |_: AgentEvent| {};
        // No name at all → rejected on name.
        let err = sa
            .run_with_emit(&json!({"prompt": "do it"}), &ctx, &mut emit)
            .unwrap_err();
        assert!(format!("{err:#}").contains("name"), "missing name: {err:#}");
        // Empty prompt → rejected on prompt.
        let err = sa
            .run_with_emit(&json!({"name": "x", "prompt": "   "}), &ctx, &mut emit)
            .unwrap_err();
        assert!(format!("{err:#}").contains("prompt"), "empty prompt: {err:#}");
    }

    #[test]
    fn child_tools_retains_by_capability_not_name() {
        // The sub-agent's tool list is the config whitelist built FULL, then
        // retained by available_to_subagent() — `task` and `todo` (both opting
        // out via the trait) are gone while leaf tools stay, in whitelist
        // order. No name literal drives the filter: a third-party tool opting
        // out is dropped the same way.
        let mut cfg = fake_cfg();
        cfg.tools = vec![
            "shell".into(),
            "todo".into(),
            "task".into(),
            "read_file".into(),
        ];
        let built = child_tools(&cfg, &reqwest::blocking::Client::new(), "k", &[]).unwrap();
        let names: Vec<String> = built.iter().map(|t| t.def().name).collect();
        assert_eq!(names, vec!["shell".to_string(), "read_file".to_string()]);
    }

    /// Trivial Hooks impl (all default methods) for the ctx-inheritance assert.
    struct NoopHooks;
    impl crate::hooks::Hooks for NoopHooks {}

    #[test]
    fn child_ctx_inherits_hooks_and_tags_subagent_name() {
        // The child's ctx Arc-CLONES the parent's hooks layer (denied at the
        // top level stays denied inside the sub-agent — no bypass), and tags
        // the child with its sub-agent name so hooks can tell its calls apart.
        // Sandbox / limit fields carry over unchanged.
        let hooks: std::sync::Arc<dyn crate::hooks::Hooks> =
            std::sync::Arc::new(NoopHooks);
        let parent = ToolCtx {
            workdir: std::env::temp_dir(),
            max_output_chars: 1234,
            restrict_to_workdir: true,
            cancel: None,
            shell_timeout_secs: 77,
            hooks: Some(hooks.clone()),
            subagent: None,
        };
        let child = child_ctx(&parent, "scout");
        assert!(
            std::sync::Arc::ptr_eq(
                parent.hooks.as_ref().unwrap(),
                child.hooks.as_ref().unwrap()
            ),
            "child must SHARE the parent's hooks layer (no bypass)"
        );
        assert_eq!(child.subagent.as_deref(), Some("scout"));
        assert_eq!(child.workdir, parent.workdir);
        assert_eq!(child.max_output_chars, 1234);
        assert!(child.restrict_to_workdir);
        assert_eq!(child.shell_timeout_secs, 77);
    }

    fn fake_cfg() -> Config {
        Config {
            provider: crate::config::ProviderKind::Anthropic,
            base_url: None,
            api_key_env: "X".into(),
            model: "m".into(),
            max_tokens: 64,
            system_prompt: None,
            task: None,
            workdir: None,
            tools: vec![],
            restrict_to_workdir: false,
            limits: crate::config::Limits::default(),
            mcp_servers: vec![],
            reasoning_effort: None,
            inject_environment: false,
        }
    }
}
