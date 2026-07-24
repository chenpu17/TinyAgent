use crate::config::{Config, Limits};
use crate::provider::{Block, ChatEvent, ChatRequest, Message, Provider, Role, ToolDef};
use crate::tools::{Tool, ToolCtx};
use anyhow::Result;
use serde_json::Value;
use std::time::{Duration, Instant};

/// Events emitted during a run; frontends (headless printer / TUI) consume them.
pub enum AgentEvent {
    TurnStart(u32),
    TextDelta(String),
    ToolCall { name: String, input: Value },
    ToolResult { name: String, is_error: bool, preview: String },
    /// History was compressed: how many middle messages were dropped,
    /// and how many remain.
    Compressed { dropped: usize, kept: usize },
    Done { stop_reason: String },
}

#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub name: String,
    pub input: Value,
    pub is_error: bool,
}

pub struct RunOutcome {
    pub final_text: String,
    pub turns: u32,
    pub stop_reason: String,
    pub tool_calls: Vec<ToolCallRecord>,
    pub elapsed: Duration,
}

pub struct Agent {
    provider: Box<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    tool_defs: Vec<ToolDef>,
    messages: Vec<Message>,
    system: Option<String>,
    model: String,
    max_tokens: u32,
    limits: Limits,
    ctx: ToolCtx,
    session_path: Option<std::path::PathBuf>,
}

impl Agent {
    pub fn new(
        cfg: &Config,
        provider: Box<dyn Provider>,
        tools: Vec<Box<dyn Tool>>,
        initial: Vec<Message>,
        ctx: ToolCtx,
    ) -> Self {
        let tool_defs = tools.iter().map(|t| t.def()).collect();
        Self {
            provider,
            tools,
            tool_defs,
            messages: initial,
            system: cfg.system_prompt.clone(),
            model: cfg.model.clone(),
            max_tokens: cfg.max_tokens,
            limits: cfg.limits.clone(),
            ctx,
            session_path: None,
        }
    }

    /// Enable per-turn session persistence to the given file.
    pub fn enable_session(&mut self, path: std::path::PathBuf) {
        self.session_path = Some(path);
    }

    fn save_session(&self) {
        if let Some(path) = &self.session_path {
            if let Err(e) = crate::session::save(path, &self.model, &self.messages) {
                eprintln!("[warn] session save failed: {e:#}");
            }
        }
    }

    /// Append a user message (multi-turn / TUI follow-ups).
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub fn push_user(&mut self, blocks: Vec<Block>) {
        self.messages.push(Message::user(blocks));
        self.save_session();
    }

    /// Run the loop until the model stops calling tools, or a limit is hit.
    pub fn run(&mut self, events: &mut dyn FnMut(AgentEvent)) -> Result<RunOutcome> {
        let start = Instant::now();
        let mut turns = 0u32;
        let mut final_text = String::new();
        let mut records: Vec<ToolCallRecord> = Vec::new();
        let mut stop_reason: String;

        loop {
            if turns >= self.limits.max_turns {
                stop_reason = "max_turns_reached".into();
                break;
            }
            if start.elapsed().as_secs() >= self.limits.timeout_secs {
                stop_reason = "timeout".into();
                break;
            }
            turns += 1;
            events(AgentEvent::TurnStart(turns));
            self.compress_if_needed(events);

            let req = ChatRequest {
                system: self.system.as_deref(),
                messages: &self.messages,
                tools: &self.tool_defs,
                model: &self.model,
                max_tokens: self.max_tokens,
            };
            let resp = self.provider.chat(&req, &mut |e| match e {
                ChatEvent::TextDelta(t) => events(AgentEvent::TextDelta(t)),
            })?;

            stop_reason = resp.stop_reason.clone();
            let has_tool_use = resp
                .blocks
                .iter()
                .any(|b| matches!(b, Block::ToolUse { .. }));
            for b in &resp.blocks {
                if let Block::Text(t) = b {
                    if !final_text.is_empty() {
                        final_text.push('\n');
                    }
                    final_text.push_str(t);
                }
            }
            self.messages.push(Message::assistant(resp.blocks.clone()));
            self.save_session();

            if !has_tool_use {
                break; // end_turn / max_tokens without pending calls
            }

            // Execute each tool call, collect results into one user message.
            let mut results: Vec<Block> = Vec::new();
            for b in &resp.blocks {
                let Block::ToolUse { id, name, input } = b else {
                    continue;
                };
                events(AgentEvent::ToolCall {
                    name: name.clone(),
                    input: input.clone(),
                });

                let output = match self.tools.iter().find(|t| t.def().name == *name) {
                    Some(tool) => match tool.run(input, &self.ctx) {
                        Ok(o) => o,
                        Err(e) => crate::tools::ToolOutput::error(format!("{e:#}")),
                    },
                    None => crate::tools::ToolOutput::error(format!("unknown tool: {name}")),
                };

                records.push(ToolCallRecord {
                    name: name.clone(),
                    input: input.clone(),
                    is_error: output.is_error,
                });
                events(AgentEvent::ToolResult {
                    name: name.clone(),
                    is_error: output.is_error,
                    preview: preview(&output.content),
                });
                results.push(Block::ToolResult {
                    tool_use_id: id.clone(),
                    content: output.content,
                    image: output.image.map(Box::new),
                    is_error: output.is_error,
                });
            }
            self.messages.push(Message::user(results));
            self.save_session();
        }

        events(AgentEvent::Done {
            stop_reason: stop_reason.clone(),
        });
        Ok(RunOutcome {
            final_text,
            turns,
            stop_reason,
            tool_calls: records,
            elapsed: start.elapsed(),
        })
    }
}

fn preview(s: &str) -> String {
    const MAX: usize = 300;
    let one_line: String = s.chars().take(MAX).collect();
    if s.chars().count() > MAX {
        format!("{one_line}…")
    } else {
        one_line
    }
}

// ─── Context compression ────────────────────────────────────────────

const SUMMARY_SYSTEM: &str = "You are compressing the earlier part of an AI agent's \
conversation so it can continue within its context limit. Write a structured summary in \
the same language as the conversation, covering: the task goal, progress made, key \
decisions, files created/modified, errors encountered and how they were resolved, and \
the current state / next step. Be concise but keep all facts the agent still needs.";

const SUMMARY_MARKER: &str =
    "[前情摘要，由系统自动生成 / Auto-generated summary of the earlier conversation]";

/// Rough size estimate of a conversation (chars; images count cheaply).
fn total_chars(messages: &[Message]) -> usize {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .map(|b| match b {
            Block::Text(t) => t.len(),
            Block::Image { data, .. } => data.len() / 100, // base64 is ~33% overhead
            Block::ToolUse { input, .. } => input.to_string().len(),
            Block::ToolResult { content, .. } => content.len(),
        })
        .sum()
}

/// Find a safe tail-start index. An assistant message is always a safe
/// boundary: its tool_use blocks (if any) are answered by the tool_result
/// that immediately follows it inside the kept tail. Cutting before a
/// tool_result-bearing user message would orphan those results, so those
/// are skipped. Must leave at least one middle message worth summarizing
/// (cut >= 2).
fn find_safe_cut(messages: &[Message], keep_recent: usize) -> Option<usize> {
    let n = messages.len();
    if n <= keep_recent + 2 {
        return None;
    }
    let mut cut = n - keep_recent;
    while cut < n {
        if messages[cut].role == Role::Assistant {
            return if cut >= 2 { Some(cut) } else { None };
        }
        cut += 1;
    }
    None
}

impl Agent {
    /// If history exceeds the configured threshold, summarize the middle
    /// via the LLM and keep only the original task + recent tail.
    /// Failures degrade gracefully (warn and continue uncompressed).
    fn compress_if_needed(&mut self, events: &mut dyn FnMut(AgentEvent)) {
        let threshold = self.limits.compress_threshold_chars;
        if threshold == 0 || total_chars(&self.messages) <= threshold {
            return;
        }
        let Some(cut) = find_safe_cut(&self.messages, self.limits.keep_recent_messages) else {
            return;
        };

        let summary = match self.summarize_prefix(cut) {
            Ok(s) if !s.trim().is_empty() => s,
            Ok(_) => return,
            Err(e) => {
                eprintln!("[warn] context compression failed: {e:#}");
                return;
            }
        };

        let dropped = cut - 1;
        let mut compressed = Vec::with_capacity(self.messages.len() - dropped + 1);
        // Fold the summary into the original task message, so the tail
        // (which may start with an assistant message) keeps proper
        // user → assistant alternation.
        let mut head = self.messages[0].clone();
        // Drop previous summary blocks: the new summary already absorbed
        // them via the transcript, so the head stays task + one summary.
        head.content
            .retain(|b| !matches!(b, Block::Text(t) if t.starts_with(SUMMARY_MARKER)));
        head.content
            .push(Block::Text(format!("{SUMMARY_MARKER}\n{summary}")));
        compressed.push(head);
        compressed.extend_from_slice(&self.messages[cut..]);
        self.messages = compressed;
        self.save_session();
        events(AgentEvent::Compressed {
            dropped,
            kept: self.messages.len(),
        });
    }

    /// Ask the LLM to summarize messages[..cut] into a compact brief.
    fn summarize_prefix(&self, cut: usize) -> Result<String> {
        let mut transcript = String::new();
        for m in &self.messages[..cut] {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            for b in &m.content {
                match b {
                    Block::Text(t) => transcript.push_str(&format!("[{role}] {t}\n")),
                    Block::Image { .. } => transcript.push_str(&format!("[{role}] <image>\n")),
                    Block::ToolUse { name, input, .. } => {
                        let args = input.to_string();
                        let args = preview(&args);
                        transcript.push_str(&format!("[{role}] tool_call {name}: {args}\n"))
                    }
                    Block::ToolResult {
                        content, is_error, ..
                    } => {
                        let body = preview(content);
                        let tag = if *is_error { "tool_error" } else { "tool_result" };
                        transcript.push_str(&format!("[{role}] {tag}: {body}\n"))
                    }
                }
            }
        }
        let req = ChatRequest {
            system: Some(SUMMARY_SYSTEM),
            messages: &[Message::user(vec![Block::Text(format!(
                "Summarize this conversation transcript:\n\n{transcript}"
            ))])],
            tools: &[],
            model: &self.model,
            max_tokens: 2048,
        };
        let resp = self.provider.chat(&req, &mut |_| {})?;
        Ok(resp
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user_text(s: &str) -> Message {
        Message::user(vec![Block::Text(s.into())])
    }

    fn tool_turn(id: &str) -> Vec<Message> {
        vec![
            Message::assistant(vec![Block::ToolUse {
                id: id.into(),
                name: "shell".into(),
                input: json!({"command": "ls"}),
            }]),
            Message::user(vec![Block::ToolResult {
                tool_use_id: id.into(),
                content: "ok".into(),
                image: None,
                is_error: false,
            }]),
        ]
    }

    #[test]
    fn safe_cut_skips_tool_pairs() {
        // [task][user a][tool pair][user b][tool pair at tail]
        let mut msgs = vec![user_text("task"), user_text("a")];
        msgs.extend(tool_turn("t1"));
        msgs.push(user_text("b"));
        msgs.extend(tool_turn("t2"));
        // indices: 0 task, 1 a, 2 asst-tu, 3 user-tr, 4 b, 5 asst-tu, 6 user-tr
        // keep_recent=2 → cut at 5 (assistant w/ tool_use); its tool_result
        // at index 6 stays in the tail — pair intact.
        assert_eq!(find_safe_cut(&msgs, 2), Some(5));
        // keep_recent=4 → candidate 3 (tool_result user) → slides past 4
        // (plain user) to 5 (assistant).
        assert_eq!(find_safe_cut(&msgs, 4), Some(5));
    }

    #[test]
    fn safe_cut_slides_past_tool_results() {
        // Pure tool loop: [task][asst tu][user tr] × 4
        let mut msgs = vec![user_text("task")];
        for i in 0..4 {
            msgs.extend(tool_turn(&format!("t{i}")));
        }
        // indices: 0 task, 1 asst, 2 user-tr, 3 asst, 4 user-tr, ...
        // keep_recent=2 → candidate cut at 7 = assistant (pair t3 intact).
        assert_eq!(find_safe_cut(&msgs, 2), Some(7));
        // keep_recent=6 → candidate cut at 3 = assistant.
        assert_eq!(find_safe_cut(&msgs, 6), Some(3));
    }

    #[test]
    fn safe_cut_too_short_history() {
        let msgs = vec![user_text("task"), user_text("hello"), user_text("more")];
        assert_eq!(find_safe_cut(&msgs, 8), None); // n <= keep_recent + 2
    }

    #[test]
    fn safe_cut_needs_middle_content() {
        // Candidate lands at index 1 (assistant) → nothing to summarize → None
        let msgs = vec![
            user_text("task"),
            Message::assistant(vec![Block::Text("r1".into())]),
            Message::assistant(vec![Block::Text("r2".into())]),
            Message::assistant(vec![Block::Text("r3".into())]),
            Message::assistant(vec![Block::Text("r4".into())]),
        ];
        assert_eq!(find_safe_cut(&msgs, 3), None);
    }

    #[test]
    fn total_chars_counts_blocks() {
        let msgs = vec![
            user_text("12345"),
            Message::assistant(vec![Block::ToolUse {
                id: "x".into(),
                name: "s".into(),
                input: json!({"a": 1}),
            }]),
        ];
        // "12345" (5) + "{\"a\":1}" (7)
        assert_eq!(total_chars(&msgs), 12);
    }
}
