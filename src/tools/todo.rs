//! The `todo` tool: the agent's self-tracked task plan.
//!
//! Full-replacement semantics (each call sends the ENTIRE plan), like
//! TodoWrite: the model re-sends the current list every time it wants to
//! reflect progress. This keeps the tool stateless (`&self`, like every leaf
//! tool) — there is no plan state on `Agent` or in `ToolCtx`. The authoritative
//! final plan is simply the `input` of the LAST non-error `todo` call in the
//! run's tool-call log, derived by [`crate::agent::extract_plan`] at run end and
//! written to `result.json`'s `plan` field. That log is independent of context
//! compression (compression rewrites `messages`, never the `records`
//! accumulator), so the plan survives even when old `todo` tool_results in
//! history are folded into the summary.
//!
//! Why a tool at all (vs. letting the model plan in prose): prose plans are
//! invisible to the result-file audit and get compressed away. A `todo` call is
//! a structured, compression-proof checkpoint — the harness reads back not just
//! WHAT the agent did (`tool_calls`) but WHAT it set out to do and how far it
//! got (`plan`). That closes a gap in the "complete, honest, machine-readable
//! bill" positioning.

use super::{Tool, ToolCtx, ToolOutput};
use crate::agent::{AgentEvent, TodoItem};
use crate::provider::ToolDef;
use anyhow::{ensure, Result};
use serde_json::{json, Value};

/// Cap on plan length, to bound context and deter a runaway model from dumping
/// a giant list. Generous enough for real plans (a few dozen steps), small
/// enough that a `todo` tool_result never becomes the dominant context cost.
const MAX_TODOS: usize = 50;
/// Cap on a single step's text — keeps the rendered checklist readable and the
/// `plan` field in `result.json` compact.
const MAX_SUBJECT_CHARS: usize = 200;

/// The `todo` tool. Stateless — every call's `input` is the full new plan.
pub struct Todo;

impl Tool for Todo {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "todo".into(),
            description: "Track your task plan. Send the FULL current list every call \
                (full-replacement, not a delta). Set it at the start and update status as you \
                progress — the final plan is written to result.json so the work is auditable. \
                statuses: pending | in_progress | completed."
                .into(),
            schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The complete current plan. Each call REPLACES the whole list (send every item, not just changes).",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "subject": {
                                    "type": "string",
                                    "description": "A concise description of this step."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "pending = not started, in_progress = working on it now, completed = done."
                                }
                            },
                            "required": ["subject", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        }
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        // Leaf path (no emit): the agent loop always calls `run_with_emit`, but
        // the trait requires `run` and downstream library callers may use it.
        self.exec(input, ctx).map(|(out, _)| out)
    }

    fn run_with_emit(
        &self,
        input: &Value,
        ctx: &ToolCtx,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<ToolOutput> {
        let (out, todos) = self.exec(input, ctx)?;
        // Stream the update so `--json` pipelines can render live progress. The
        // final plan is also derived into result.json at run end, so this is for
        // real-time visibility, not the authoritative record.
        emit(AgentEvent::TodoUpdate { todos });
        Ok(out)
    }

    fn available_to_subagent(&self) -> bool {
        // Plan tracking is the PARENT's job: a sub-agent is a focused short
        // task, and a child's `todo` calls would never reach the parent's
        // result.json `plan` field anyway (extract_plan reads the PARENT's
        // tool-call log) — they'd just burn tokens.
        false
    }
}

impl Todo {
    /// Parse + validate the input, returning the tool output plus the typed
    /// plan (for the caller to emit as a `TodoUpdate`). Errors on any
    /// validation failure — so a `todo` record that lands in the tool-call log
    /// as non-error is guaranteed well-formed, which `extract_plan` relies on.
    fn exec(&self, input: &Value, _ctx: &ToolCtx) -> Result<(ToolOutput, Vec<TodoItem>)> {
        let arr = input
            .get("todos")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("todo requires a `todos` array"))?;
        ensure!(
            arr.len() <= MAX_TODOS,
            "todo: too many items ({}, limit {MAX_TODOS})",
            arr.len()
        );
        let mut items = Vec::with_capacity(arr.len());
        for v in arr {
            let subject = v
                .get("subject")
                .and_then(|s| s.as_str())
                .ok_or_else(|| anyhow::anyhow!("todo: each item needs a string `subject`"))?;
            let subject = subject.trim();
            ensure!(!subject.is_empty(), "todo: item `subject` must not be empty");
            ensure!(
                subject.chars().count() <= MAX_SUBJECT_CHARS,
                "todo: item `subject` too long (limit {MAX_SUBJECT_CHARS} chars)"
            );
            let status = v
                .get("status")
                .and_then(|s| s.as_str())
                .ok_or_else(|| anyhow::anyhow!("todo: each item needs a string `status`"))?;
            ensure!(
                matches!(status, "pending" | "in_progress" | "completed"),
                "todo: `status` must be pending | in_progress | completed (got {status:?})"
            );
            items.push(TodoItem {
                subject: subject.to_string(),
                status: status.to_string(),
            });
        }
        Ok((ToolOutput::text(render(&items)), items))
    }
}

/// Human-readable checklist the model sees in the tool_result — its latest plan
/// snapshot. Glyphs: `☐` pending · `▶` in_progress · `✓` completed. Kept compact
/// so repeated `todo` calls don't bloat history; the model tracks progress from
/// this view, while the structured audit truth is the call's `input`.
fn render(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "[plan cleared]".into();
    }
    let mut s = String::with_capacity(items.len() * 32);
    for it in items {
        let glyph = match it.status.as_str() {
            "completed" => "✓",
            "in_progress" => "▶",
            _ => "☐",
        };
        s.push_str(glyph);
        s.push(' ');
        s.push_str(&it.subject);
        s.push('\n');
    }
    s.pop(); // drop trailing newline
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(subject: &str, status: &str) -> Value {
        json!({"subject": subject, "status": status})
    }

    #[test]
    fn accepts_well_formed_plan() {
        let (out, todos) = Todo
            .exec(
                &json!({"todos": [item("a", "pending"), item("b", "completed")]}),
                &test_ctx(),
            )
            .unwrap();
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].subject, "a");
        assert_eq!(todos[1].status, "completed");
        // Rendering marks each row with the right glyph.
        assert!(out.content.contains("☐ a"));
        assert!(out.content.contains("✓ b"));
    }

    #[test]
    fn empty_list_clears_plan() {
        let (out, todos) = Todo.exec(&json!({"todos": []}), &test_ctx()).unwrap();
        assert!(todos.is_empty());
        assert_eq!(out.content, "[plan cleared]");
    }

    #[test]
    fn full_replacement_not_delta() {
        // First call: 3 items. Second call: 1 item. The returned plan reflects
        // ONLY the second call — exec is stateless and returns exactly the
        // input list, never merges with prior state.
        let (_, first) = Todo
            .exec(
                &json!({"todos": [item("a", "pending"), item("b", "pending"), item("c", "pending")]}),
                &test_ctx(),
            )
            .unwrap();
        assert_eq!(first.len(), 3);
        let (_, second) = Todo
            .exec(&json!({"todos": [item("only", "in_progress")]}), &test_ctx())
            .unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].subject, "only");
    }

    #[test]
    fn rejects_bad_status() {
        let err = Todo
            .exec(&json!({"todos": [item("a", "done")]}), &test_ctx())
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("pending | in_progress | completed"),
            "{err:#}"
        );
    }

    #[test]
    fn rejects_missing_subject() {
        let err = Todo
            .exec(&json!({"todos": [{"status": "pending"}]}), &test_ctx())
            .unwrap_err();
        assert!(format!("{err:#}").contains("subject"), "{err:#}");
    }

    #[test]
    fn rejects_blank_subject() {
        let err = Todo
            .exec(&json!({"todos": [item("   ", "pending")]}), &test_ctx())
            .unwrap_err();
        assert!(format!("{err:#}").contains("empty"), "{err:#}");
    }

    #[test]
    fn rejects_missing_todos_array() {
        assert!(Todo.exec(&json!({}), &test_ctx()).is_err());
        assert!(Todo.exec(&json!({"todos": "notarray"}), &test_ctx()).is_err());
    }

    #[test]
    fn rejects_too_many_items() {
        let many: Vec<Value> = (0..MAX_TODOS + 1)
            .map(|i| item(&format!("step{i}"), "pending"))
            .collect();
        let err = Todo
            .exec(&json!({"todos": many}), &test_ctx())
            .unwrap_err();
        assert!(format!("{err:#}").contains("too many"), "{err:#}");
    }

    #[test]
    fn renders_all_three_glyphs() {
        let (out, _) = Todo
            .exec(
                &json!({"todos": [
                    item("todo", "pending"),
                    item("doing", "in_progress"),
                    item("done", "completed")
                ]}),
                &test_ctx(),
            )
            .unwrap();
        assert!(out.content.contains("☐ todo"));
        assert!(out.content.contains("▶ doing"));
        assert!(out.content.contains("✓ done"));
    }

    /// Minimal ToolCtx for unit-level exec (no cancel, no workdir needs).
    fn test_ctx() -> ToolCtx {
        ToolCtx {
            workdir: std::env::temp_dir(),
            max_output_chars: 30000,
            restrict_to_workdir: false,
            cancel: None,

            shell_timeout_secs: 120,
        }
    }
}
