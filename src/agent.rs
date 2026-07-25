use crate::config::{Config, Limits};
use crate::provider::{Block, ChatEvent, ChatRequest, Message, Provider, Role, ToolDef};
use crate::tools::{Tool, ToolCtx};
use anyhow::Result;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Events emitted during a run; frontends (headless printer / TUI) consume them.
pub enum AgentEvent {
    TurnStart(u32),
    TextDelta(String),
    ToolCall { name: String, input: Value },
    ToolResult { name: String, is_error: bool, content: String },
    /// Token usage reported by the provider for the latest chat request.
    /// Best-effort: emitted only when the provider actually reported usage
    /// (e.g. the stream wasn't cut off before the usage frame).
    Usage { input_tokens: u64, output_tokens: u64 },
    /// History was compressed: how many middle messages were dropped,
    /// and how many remain.
    Compressed { dropped: usize, kept: usize },
    Done { stop_reason: String },
}

#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub name: String,
    pub input: Value,
    /// The tool's result text (what read_file returned, shell stdout, …) — kept
    /// so the structured result file is self-contained for verification, not
    /// just the model's summary of it.
    pub output: String,
    pub is_error: bool,
}

pub struct RunOutcome {
    pub final_text: String,
    pub turns: u32,
    pub stop_reason: String,
    pub tool_calls: Vec<ToolCallRecord>,
    /// Token usage summed across every chat request in this run (best-effort:
    /// only the requests whose stream reached the usage frame).
    pub input_tokens: u64,
    pub output_tokens: u64,
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
    /// Set by the frontend (e.g. TUI Esc) to stop at the next turn boundary.
    cancel: Option<Arc<AtomicBool>>,
    /// One compression failure disables further attempts for this run,
    /// so a persistent error doesn't retry (and warn) every single turn.
    compress_failed: bool,
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
            cancel: None,
            compress_failed: false,
        }
    }

    /// Attach a cancellation flag; checked at every turn boundary and shared
    /// with tools (via ToolCtx) so a running shell can be interrupted too,
    /// not only the streaming read.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub fn set_cancel(&mut self, flag: Arc<AtomicBool>) {
        self.ctx.cancel = Some(flag.clone());
        self.cancel = Some(flag);
    }

    fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Relaxed))
    }

    /// Whether the last message is a user message. The TUI uses this to
    /// decide whether to kick off an initial run — an initial task baked
    /// in by main, or a resumed session that stopped mid-turn at a
    /// tool_result — instead of waiting for the user to type something.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub fn ends_with_user(&self) -> bool {
        self.messages.last().is_some_and(|m| m.role == Role::User)
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
        append_user_turn(&mut self.messages, blocks);
        self.save_session();
    }

    /// Drop the conversation transcript (`/clear`). Keeps config, tools, and
    /// system prompt — only messages are reset, and the session file is
    /// rewritten so a restart doesn't restore the cleared chat.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub fn clear_messages(&mut self) {
        self.messages.clear();
        self.save_session();
    }

    /// Run the loop until the model stops calling tools, or a limit is hit.
    pub fn run(&mut self, events: &mut dyn FnMut(AgentEvent)) -> Result<RunOutcome> {
        let start = Instant::now();
        // Each top-level run gets a fresh compression budget. In the TUI the
        // same Agent is reused across follow-ups; without this reset, one
        // transient compression failure would disable it for the rest of the
        // session and the history would grow until it hit the model's limit.
        self.compress_failed = false;
        // Reset any stale cancel from a previous run: cancel is level-triggered
        // and never auto-clears elsewhere, so without this a Ctrl-C that ended
        // the last turn would cancel this one before it starts.
        if let Some(c) = &self.cancel {
            c.store(false, Ordering::Relaxed);
        }
        // Overall run deadline. Each chat is capped to the time left so a
        // single stuck request can't outlive the run budget — which is
        // otherwise only enforced at turn boundaries.
        let deadline = (self.limits.timeout_secs > 0)
            .then(|| start + Duration::from_secs(self.limits.timeout_secs));
        let mut turns = 0u32;
        let mut final_text = String::new();
        let mut records: Vec<ToolCallRecord> = Vec::new();
        let mut stop_reason: String;
        // Running token totals for RunOutcome (the per-turn Usage event is also
        // emitted to the frontend; these accumulate for the result file).
        let mut in_tokens: u64 = 0;
        let mut out_tokens: u64 = 0;

        loop {
            if self.cancelled() {
                stop_reason = "cancelled".into();
                break;
            }
            if turns >= self.limits.max_turns {
                stop_reason = "max_turns_reached".into();
                break;
            }
            // timeout_secs == 0 means "no timeout" (mirroring the deadline
            // guard below). Without this check, elapsed() >= 0 is always true
            // and the agent would time out before its very first turn.
            if self.limits.timeout_secs > 0
                && start.elapsed().as_secs() >= self.limits.timeout_secs
            {
                stop_reason = "timeout".into();
                break;
            }
            turns += 1;
            events(AgentEvent::TurnStart(turns));
            self.compress_if_needed(events, deadline);

            let req = ChatRequest {
                system: self.system.as_deref(),
                messages: &self.messages,
                tools: &self.tool_defs,
                model: &self.model,
                max_tokens: self.max_tokens,
                per_request_timeout: deadline
                    .map(|d| d.saturating_duration_since(Instant::now())),
                // Shared cancel flag: the streaming reader checks it between
                // chunks, so Esc/Ctrl-C can interrupt a stuck request mid-read
                // (not only at the next turn boundary). Headless has no flag →
                // a dummy never-set one, so idle-timeout still applies.
                cancel: self
                    .cancel
                    .clone()
                    .unwrap_or_else(|| Arc::new(AtomicBool::new(false))),
            };
            let resp = self.provider.chat(&req, &mut |e| match e {
                ChatEvent::TextDelta(t) => events(AgentEvent::TextDelta(t)),
            })?;
            if let Some(u) = resp.usage {
                in_tokens += u.input_tokens;
                out_tokens += u.output_tokens;
                events(AgentEvent::Usage {
                    input_tokens: u.input_tokens,
                    output_tokens: u.output_tokens,
                });
            }

            stop_reason = resp.stop_reason.clone();
            let has_tool_use = resp
                .blocks
                .iter()
                .any(|b| matches!(b, Block::ToolUse { .. }));
            // The result summary keeps only the last turn's text — tool-loop
            // chatter from earlier turns is transcript, not conclusion.
            let mut turn_text = String::new();
            for b in &resp.blocks {
                if let Block::Text(t) = b {
                    if !turn_text.is_empty() {
                        turn_text.push('\n');
                    }
                    turn_text.push_str(t);
                }
            }
            if !turn_text.is_empty() {
                final_text = turn_text;
            }
            self.messages.push(Message::assistant(resp.blocks.clone()));
            self.save_session();

            // A cancelled / idle-timeout / truncated / max_tokens stream may
            // leave a half-delivered tool_use whose input is partial or _raw.
            // OpenAI maps finish_reason "length" → "max_tokens"; when the
            // budget runs out mid tool-call arguments the truncated JSON fails
            // to parse and falls back to a _raw sentinel — executing that is
            // garbage. Anthropic is unaffected (it still emits content_block_stop,
            // so malformed JSON fails the request rather than yielding _raw).
            // Drop such tool_use blocks; also remove them from the saved
            // assistant message so a tool_use with no matching tool_result
            // isn't rejected by strict APIs on the next request. Keep whatever
            // partial text arrived so the transcript reflects what the user
            // saw. (stop_reason is still reported as-is below.)
            let aborted = self.cancelled()
                || matches!(
                    resp.stop_reason.as_str(),
                    "cancelled" | "stream_idle" | "stream_truncated" | "max_tokens"
                );
            if aborted {
                let kept: Vec<Block> = resp
                    .blocks
                    .iter()
                    .filter(|b| !matches!(b, Block::ToolUse { .. }))
                    .cloned()
                    .collect();
                // Replace the just-pushed message with the filtered one (or
                // drop it entirely if only a partial tool_use arrived).
                match self.messages.last_mut() {
                    Some(m) if m.role == Role::Assistant => {
                        if kept.is_empty() {
                            self.messages.pop();
                        } else {
                            m.content = kept;
                        }
                    }
                    _ => {}
                }
                self.save_session();
                break;
            }

            if !has_tool_use {
                break; // end_turn / max_tokens without pending calls
            }

            // Execute each tool call, collect results into one user message.
            // Once cancelled (e.g. Ctrl-C during tool #1), stop running further
            // tools — but STILL emit a result for every remaining tool_use: an
            // assistant message carrying N tool_use blocks must be followed by
            // N tool_results, or the next request is rejected. (A non-cancel-
            // aware tool that was already in flight returns on its own; later
            // tools in the batch are short-circuited here.)
            let mut results: Vec<Block> = Vec::new();
            let mut batch_cancelled = false;
            for b in &resp.blocks {
                let Block::ToolUse { id, name, input } = b else {
                    continue;
                };
                if batch_cancelled || self.cancelled() {
                    batch_cancelled = true;
                    results.push(Block::ToolResult {
                        tool_use_id: id.clone(),
                        content: "[cancelled by user]".into(),
                        image: None,
                        is_error: true,
                    });
                    continue;
                }
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
                    output: output.content.clone(),
                    is_error: output.is_error,
                });
                events(AgentEvent::ToolResult {
                    name: name.clone(),
                    is_error: output.is_error,
                    content: output.content.clone(),
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
            input_tokens: in_tokens,
            output_tokens: out_tokens,
            elapsed: start.elapsed(),
        })
    }
}

pub(crate) fn preview(s: &str) -> String {
    const MAX: usize = 200;
    // Single pass: take MAX chars, then test whether anything remained. The
    // old `.chars().count() > MAX` walked the WHOLE string just to decide
    // truncation — O(n) per call, and the renderer calls this every frame for
    // every collapsed tool row.
    let mut it = s.chars();
    let head: String = it.by_ref().take(MAX).collect();
    if it.next().is_some() {
        format!("{head}…")
    } else {
        head
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

/// Time reserved for the main chat request after a compression summary, so the
/// summary can't eat the whole remaining run budget and leave the main request
/// with a near-zero timeout (which would fail the whole run with no result).
const COMPRESS_RESERVE: Duration = Duration::from_secs(20);

/// Rough size estimate of a conversation (chars; images count cheaply).
fn total_chars(messages: &[Message]) -> usize {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .map(|b| match b {
            Block::Text(t) => t.len(),
            Block::Image { data, .. } => data.len() / 100, // base64 is ~33% overhead
            Block::ToolUse { input, .. } => input.to_string().len(),
            // A tool result may carry an image payload (e.g. load_image).
            // Count it like a top-level image, or a load_image-heavy
            // conversation never trips the compress threshold.
            Block::ToolResult { content, image, .. } => {
                let img = image
                    .as_deref()
                    .map(|b| match b {
                        Block::Image { data, .. } => data.len() / 100,
                        _ => 0,
                    })
                    .unwrap_or(0);
                content.len() + img
            }
        })
        .sum()
}

/// Build the compressed history: original task (with the summary folded in
/// as one extra text block) + the untouched tail. Folding into message[0]
/// keeps proper user → assistant alternation when the tail starts with an
/// assistant message. Previous summary blocks are replaced, not stacked —
/// the new summary already absorbed them via the transcript.
fn fold_summary(messages: &[Message], cut: usize, summary: &str) -> Vec<Message> {
    let mut compressed = Vec::with_capacity(messages.len() - (cut - 1) + 1);
    let mut head = messages[0].clone();
    head.content
        .retain(|b| !matches!(b, Block::Text(t) if t.starts_with(SUMMARY_MARKER)));
    head.content
        .push(Block::Text(format!("{SUMMARY_MARKER}\n{summary}")));
    compressed.push(head);
    compressed.extend_from_slice(&messages[cut..]);
    compressed
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

/// Fold `blocks` into a trailing user message, or push a new one. Two adjacent
/// user messages make Anthropic reject the request with a 400, so a follow-up
/// must merge into the prior user turn — notably after a turn cancelled before
/// any assistant text (the assistant message is dropped, leaving history ending
/// in the prior user message this follow-up must merge into).
fn append_user_turn(messages: &mut Vec<Message>, blocks: Vec<Block>) {
    if messages.last().is_some_and(|m| m.role == Role::User) {
        if let Some(last) = messages.last_mut() {
            last.content.extend(blocks);
        }
    } else {
        messages.push(Message::user(blocks));
    }
}
impl Agent {
    /// If history exceeds the configured threshold, summarize the middle
    /// via the LLM and keep only the original task + recent tail.
    /// Failures degrade gracefully (warn and continue uncompressed).
    fn compress_if_needed(
        &mut self,
        events: &mut dyn FnMut(AgentEvent),
        deadline: Option<Instant>,
    ) {
        let threshold = self.limits.compress_threshold_chars;
        if self.compress_failed || threshold == 0 || total_chars(&self.messages) <= threshold {
            return;
        }
        // Compression is best-effort: never let the summary request eat the
        // run's remaining budget — that would leave the main request with a
        // zero/near-zero timeout and fail the whole run with no result file.
        // Skip unless there's room for both the summary and the main request.
        if let Some(d) = deadline {
            if d.saturating_duration_since(Instant::now())
                < COMPRESS_RESERVE + Duration::from_secs(5)
            {
                return;
            }
        }
        let Some(cut) = find_safe_cut(&self.messages, self.limits.keep_recent_messages) else {
            return;
        };

        let summary = match self.summarize_prefix(cut, deadline) {
            Ok(s) if !s.trim().is_empty() => s,
            Ok(_) => {
                self.compress_failed = true;
                return;
            }
            Err(e) => {
                self.compress_failed = true;
                eprintln!("[warn] context compression failed (disabled for this run): {e:#}");
                return;
            }
        };

        let dropped = cut - 1;
        self.messages = fold_summary(&self.messages, cut, &summary);
        self.save_session();
        events(AgentEvent::Compressed {
            dropped,
            kept: self.messages.len(),
        });
    }

    /// Ask the LLM to summarize messages[..cut] into a compact brief.
    fn summarize_prefix(&self, cut: usize, deadline: Option<Instant>) -> Result<String> {
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
            // Cap the summary to the remaining budget MINUS a reserve for the
            // main request that follows in the same turn, so summarizing can't
            // starve the main chat into a zero-timeout failure.
            per_request_timeout: deadline.map(|d| {
                d.saturating_duration_since(Instant::now())
                    .saturating_sub(COMPRESS_RESERVE)
            }),
            cancel: self
                .cancel
                .clone()
                .unwrap_or_else(|| Arc::new(AtomicBool::new(false))),
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
    fn append_user_turn_merges_into_trailing_user() {
        // History ends in a user message (e.g. the prior turn was cancelled
        // before any assistant text, so its assistant message was dropped).
        // The follow-up must merge, not append a second user message — two
        // adjacent user messages are a 400.
        let mut msgs = vec![user_text("task")];
        append_user_turn(&mut msgs, vec![Block::Text("again".into())]);
        assert_eq!(msgs.len(), 1, "no second user message");
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[0].content.len(), 2, "blocks merged into the one turn");
    }

    #[test]
    fn append_user_turn_pushes_new_after_assistant() {
        // Normal turn boundary: assistant spoke, now a user follow-up starts a
        // new message (no trailing user to merge into).
        let mut msgs = vec![
            user_text("task"),
            Message::assistant(vec![Block::Text("hi".into())]),
        ];
        append_user_turn(&mut msgs, vec![Block::Text("more".into())]);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2].role, Role::User);
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
    fn fold_summary_replaces_old_summary_keeps_tail() {
        // Simulate a second compression: head already carries a summary
        // block; folding a new one must replace it, not stack.
        let mut msgs = vec![user_text("task"), user_text("old turn")];
        msgs = fold_summary(&msgs, 2, "summary-one");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content.len(), 2); // task + summary

        msgs.extend(tool_turn("t9"));
        let cut = 1; // drop everything between head and the tail
        let out = fold_summary(&msgs, cut, "summary-two");
        // head + 2 tail messages
        assert_eq!(out.len(), 3);
        // head: task + exactly one summary block, and it's the new one
        let summaries: Vec<_> = out[0]
            .content
            .iter()
            .filter(|b| matches!(b, Block::Text(t) if t.starts_with(SUMMARY_MARKER)))
            .collect();
        assert_eq!(summaries.len(), 1);
        assert!(matches!(summaries[0], Block::Text(t) if t.contains("summary-two")));
        assert!(matches!(&out[0].content[0], Block::Text(t) if t == "task"));
        // tail untouched
        assert_eq!(out[1].role, Role::Assistant);
        assert_eq!(out[2].role, Role::User);
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

    #[test]
    fn total_chars_counts_tool_result_image() {
        // A load_image result carries its image inside ToolResult.image;
        // that payload must count toward the compress threshold just like
        // a top-level Block::Image, or image-heavy chats never compress.
        let msgs = vec![Message::user(vec![
            Block::Text("note".into()), // 4
            Block::ToolResult {
                tool_use_id: "x".into(),
                content: "see image".into(), // 9
                image: Some(Box::new(Block::Image {
                    media_type: "image/png".into(),
                    data: "A".repeat(1000), // 1000 / 100 = 10
                })),
                is_error: false,
            },
        ])];
        // 4 + 9 + 10 = 23
        assert_eq!(total_chars(&msgs), 23);
    }

    #[test]
    fn preview_truncates_in_one_pass() {
        // Single-pass truncation: up to 200 chars kept verbatim, 201+ gets an
        // ellipsis; the exact boundary (MAX) must NOT add one.
        assert_eq!(preview("hi"), "hi");
        let exact: String = "b".repeat(200);
        let p = preview(&exact);
        assert_eq!(p.chars().count(), 200);
        assert!(!p.ends_with('…'));
        let over = preview(&"a".repeat(500));
        assert!(over.ends_with('…'));
        assert_eq!(over.chars().count(), 201); // 200 + ellipsis
    }
}
