//! The `task` tool: a sequential sub-agent.
//!
//! The parent agent delegates a scoped piece of work to a fresh child agent
//! (its own history + turn budget), then receives only the child's final
//! summary as the tool result — the parent's context stays uncluttered by the
//! child's scratch work. The child's token usage is folded into the parent's
//! totals (honest cost) via `ToolOutput::usage`.
//!
//! v1 scope (deliberately tight):
//! - **Sequential.** A `task` call blocks the parent's turn until the child
//!   finishes; only one sub-agent runs at a time. Parallel sub-agents + workdir
//!   isolation are a later step.
//! - **No recursion.** The child's tool list is the parent's whitelist with
//!   `task` itself removed, so a sub-agent cannot spawn its own sub-agent. This
//!   sidesteps depth bookkeeping entirely.
//! - **No event forwarding.** The child streams into a no-op sink; the parent
//!   just sees the `task` tool call and its result. (TUI/NDJSON don't show
//!   child-internal progress in v1.)
//! - **Shared workdir + cancel.** The child inherits the parent's `workdir`,
//!   `restrict_to_workdir`, and cancel flag (so Ctrl-C interrupts the child
//!   mid-stream, not only at the next parent turn). Safe because the parent
//!   checks `cancelled()` before each tool call, so it never enters the child
//!   with the flag already set — the child's run-entry reset can't clobber a
//!   cancel the parent is acting on.

use crate::agent::{Agent, RunOutcome};
use crate::config::Config;
use crate::provider::{self, Block, Message, Usage};
use crate::tools::{self, Tool, ToolCtx, ToolOutput};
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
pub struct SubAgent {
    cfg: Config,
    client: reqwest::blocking::Client,
    api_key: String,
}

impl SubAgent {
    pub fn new(cfg: Config, client: reqwest::blocking::Client, api_key: String) -> Self {
        Self {
            cfg,
            client,
            api_key,
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
                    "description": {
                        "type": "string",
                        "description": "A short (3-5 word) label for what this task does."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The full, self-contained instruction for the sub-agent."
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let prompt = input["prompt"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("task requires a non-empty `prompt`"))?;
        // `description` is informational only in v1 (no event forwarding); we
        // accept it but don't consume it, so callers following the common
        // {description, prompt} shape work without extra plumbing.

        // Child config: sub-agent persona + a capped turn budget.
        let mut child_cfg = self.cfg.clone();
        child_cfg.system_prompt = Some(SUBAGENT_SYSTEM_PROMPT.into());
        child_cfg.limits.max_turns = child_cfg.limits.max_turns.min(SUBAGENT_MAX_TURNS);

        // Child tools: parent's whitelist minus `task` → no recursion. MCP
        // servers are inherited (a sub-agent may need the same external tools).
        let child_tool_names: Vec<String> = child_cfg
            .tools
            .iter()
            .filter(|t| t.as_str() != "task")
            .cloned()
            .collect();
        let mut child_tools = tools::build(&child_tool_names, &self.cfg, &self.client, &self.api_key)?;
        if !child_cfg.mcp_servers.is_empty() {
            child_tools.extend(tools::mcp::build(&child_cfg.mcp_servers)?);
        }

        let provider = provider::build(&child_cfg, &self.api_key, self.client.clone());
        let child_ctx = ToolCtx {
            workdir: ctx.workdir.clone(),
            max_output_chars: ctx.max_output_chars,
            restrict_to_workdir: ctx.restrict_to_workdir,
            cancel: ctx.cancel.clone(),
        };
        let initial = vec![Message::user(vec![Block::Text(prompt.to_string())])];
        let mut child = Agent::new(&child_cfg, provider, child_tools, initial, child_ctx);
        if let Some(flag) = ctx.cancel.clone() {
            child.set_cancel(flag);
        }
        Ok(run_subagent(&mut child))
    }
}

/// Drive a constructed child agent to completion and map its outcome to a tool
/// result. Split out from `run` so it's unit-testable with an in-process
/// `MockProvider` (no HTTP): build any `Agent`, hand it here, assert the
/// `ToolOutput`. The child's events stream into a no-op sink (v1: no forwarding).
fn run_subagent(agent: &mut Agent) -> ToolOutput {
    match agent.run(&mut |_| {}) {
        Ok(o) => outcome_to_output(&o),
        Err(e) => ToolOutput::error(format!("subagent failed: {e:#}")),
    }
}

/// Turn a completed child run into the parent-facing tool result: the child's
/// final summary as content, its usage billed to the parent, and a non-alarming
/// note when it ended short of `end_turn` (max_turns / timeout / cancelled).
fn outcome_to_output(o: &RunOutcome) -> ToolOutput {
    let content = if o.stop_reason == "end_turn" {
        o.final_text.clone()
    } else {
        format!("[subagent ended: {}]\n{}", o.stop_reason, o.final_text)
    };
    ToolOutput {
        content,
        image: None,
        // A short stop isn't necessarily a failure (partial work may be useful),
        // so don't flag is_error — the prefix note above tells the parent what
        // happened. Only a provider Err (handled in run_subagent) is_error=true.
        is_error: false,
        usage: Some(Usage {
            input_tokens: o.input_tokens,
            output_tokens: o.output_tokens,
            cache_creation_input_tokens: o.cache_creation_input_tokens,
            cache_read_input_tokens: o.cache_read_input_tokens,
        }),
    }
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
        let out = run_subagent(&mut agent);
        assert_eq!(out.content, "done summary");
        assert!(!out.is_error);
        let u = out.usage.expect("usage billed to parent");
        assert_eq!(u.input_tokens, 150);
        assert_eq!(u.output_tokens, 8);
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
        let out = outcome_to_output(&o);
        assert_eq!(out.content, "all done");
        assert!(!out.is_error);
        let u = out.usage.expect("usage billed");
        assert_eq!((u.input_tokens, u.output_tokens), (100, 5));
    }

    #[test]
    fn outcome_to_output_short_stop_notes_reason_not_error() {
        // max_turns / timeout / cancelled: surface the reason as a prefix, but
        // don't mark is_error — partial work may still be useful to the parent.
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
        let out = outcome_to_output(&o);
        assert!(!out.is_error, "short stop must not be is_error");
        assert!(out.content.starts_with("[subagent ended: max_turns_reached]"));
        assert!(out.content.contains("partial"));
    }

    #[test]
    fn subagent_tool_rejects_empty_prompt() {
        // The tool must validate before building anything: an empty prompt is a
        // caller bug, not a sub-agent run to attempt.
        let sa = SubAgent::new(
            fake_cfg(),
            reqwest::blocking::Client::new(),
            "k".into(),
        );
        let ctx = ToolCtx {
            workdir: std::env::temp_dir(),
            max_output_chars: 1000,
            restrict_to_workdir: false,
            cancel: None,
        };
        let err = sa.run(&json!({"prompt": "   "}), &ctx).unwrap_err();
        assert!(format!("{err:#}").contains("non-empty"));
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
        }
    }
}
