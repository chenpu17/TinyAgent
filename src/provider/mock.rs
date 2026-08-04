//! Test-only scripted [`Provider`]: serves a fixed sequence of responses (or
//! injected errors) and records each request, so the agent loop's invariants
//! can be asserted in-process — no HTTP server, no Python mock.
//!
//! The provider is consumed by `Agent::new` (which takes `Box<dyn Provider>`),
//! so the recorded state lives behind an `Arc` the test clones before boxing;
//! after the run it reads `state.captured` / `state.calls()`.
use super::*;
use anyhow::{anyhow, Result};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// One scripted outcome of a `chat()` call: either a completed response or an
/// injected error (consumed in FIFO order).
pub(crate) enum MockTurn {
    Ok(ChatResponse),
    Err(String),
}

/// What a `chat()` call saw — enough to assert the loop's structural
/// invariants (message count, N tool_use ↔ N tool_result pairing).
#[derive(Clone)]
pub(crate) struct Captured {
    pub(crate) message_count: usize,
    pub(crate) last_role: Option<Role>,
    /// Role of every message in the request, in order — lets tests assert no
    /// two adjacent messages share a role (Anthropic rejects that with a 400).
    pub(crate) roles: Vec<Role>,
    /// Every `tool_use` id in the request, in message/block order, across ALL
    /// messages. Paired with `tool_result_ids`, this lets a test enforce the
    /// Anthropic N:N invariant — each `tool_use` must have exactly one matching
    /// `tool_result` — over the WHOLE history (e.g. after context compression
    /// folds the prefix), not just the trailing message.
    pub(crate) tool_use_ids: Vec<String>,
    /// Every `tool_result`'s `tool_use_id` in the request, in order. A
    /// multiset-equal check against `tool_use_ids` is the N:N bijection.
    pub(crate) tool_result_ids: Vec<String>,
    /// tool_use blocks on the trailing assistant message. Captured for
    /// symmetry with `trailing_tool_results` and ad-hoc debugging; the
    /// cross-history N:N check lives in `tool_use_ids`/`tool_result_ids`.
    #[allow(dead_code)]
    pub(crate) trailing_tool_uses: usize,
    /// tool_result blocks on the trailing user message.
    pub(crate) trailing_tool_results: usize,
    /// Every `Block::Text` body in a USER-role message, in order. The summary /
    /// combine calls build a single user message carrying their prompt, so this
    /// lets a test assert what the summarizer was fed (a running-summary update
    /// vs. fresh transcript, chunk segments vs. the combined brief).
    pub(crate) user_texts: Vec<String>,
    /// The `cache` flag on the request — lets tests assert that one-off calls
    /// (the compression summary) request `cache:false` while agent turns send
    /// `cache:true`. The mock doesn't build an HTTP body, so this records the
    /// agent's intent rather than the wire-level cache_control markup.
    pub(crate) cache: bool,
    /// The per-request timeout the agent set. Lets a test assert the
    /// reader-thread leak backstop is always present (a streaming request is
    /// never sent with `None`, which would let the sse-reader worker thread
    /// linger forever on a half-open connection). See `agent::STREAM_BACKSTOP`.
    pub(crate) per_request_timeout: Option<std::time::Duration>,
}

/// Shared, interior-mutable state — cloned by the test so it survives the
/// provider being moved into the agent.
pub(crate) struct MockState {
    queue: Mutex<VecDeque<MockTurn>>,
    captured: Mutex<Vec<Captured>>,
}

impl MockState {
    pub(crate) fn calls(&self) -> usize {
        self.captured.lock().unwrap().len()
    }
    pub(crate) fn captured(&self) -> Vec<Captured> {
        // Captured is Clone — snapshot the whole vec in one call rather than
        // hand-copying each field (which silently drops new fields when someone
        // forgets to update the mapping; adding `roles` already bit us once).
        self.captured.lock().unwrap().clone()
    }
}

pub(crate) struct MockProvider {
    state: Arc<MockState>,
}

impl MockProvider {
    /// Returns the provider (to box into an agent) and a handle to its shared
    /// state (to assert on after the run).
    pub(crate) fn new(turns: Vec<MockTurn>) -> (Self, Arc<MockState>) {
        let state = Arc::new(MockState {
            queue: Mutex::new(turns.into()),
            captured: Mutex::new(Vec::new()),
        });
        (MockProvider { state: state.clone() }, state)
    }
}

impl Provider for MockProvider {
    fn chat(&self, req: &ChatRequest, on_event: &mut dyn FnMut(ChatEvent)) -> Result<ChatResponse> {
        // Capture structural facts about the request for assertions.
        let (last_role, trailing_tool_uses, trailing_tool_results) = match req.messages.last() {
            Some(m) => {
                let tu = m
                    .content
                    .iter()
                    .filter(|b| matches!(b, Block::ToolUse { .. }))
                    .count();
                let tr = m
                    .content
                    .iter()
                    .filter(|b| matches!(b, Block::ToolResult { .. }))
                    .count();
                (Some(m.role), tu, tr)
            }
            None => (None, 0, 0),
        };
        // tool_use ↔ tool_result ids across the WHOLE request (not just the
        // trailing message), so tests can assert the N:N bijection survives
        // transformations like context compression.
        let mut tool_use_ids = Vec::new();
        let mut tool_result_ids = Vec::new();
        let mut user_texts = Vec::new();
        for m in req.messages {
            for b in &m.content {
                match b {
                    Block::ToolUse { id, .. } => tool_use_ids.push(id.clone()),
                    Block::ToolResult { tool_use_id, .. } => {
                        tool_result_ids.push(tool_use_id.clone())
                    }
                    // Capture user-message text so tests can assert what the
                    // summarizer (single/chunk/combine) was fed. Assistant text
                    // and tool results aren't needed for those assertions.
                    Block::Text(t) if m.role == Role::User => user_texts.push(t.clone()),
                    _ => {}
                }
            }
        }
        self.state.captured.lock().unwrap().push(Captured {
            message_count: req.messages.len(),
            last_role,
            roles: req.messages.iter().map(|m| m.role).collect(),
            trailing_tool_uses,
            trailing_tool_results,
            tool_use_ids,
            tool_result_ids,
            user_texts,
            cache: req.cache,
            per_request_timeout: req.per_request_timeout,
        });

        // Pop the next scripted turn; running off the end is a test bug.
        let turn = self
            .state
            .queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| MockTurn::Err("mock: scripted turns exhausted".into()));
        match turn {
            MockTurn::Ok(resp) => {
                // Stream text blocks through the event callback, mirroring a
                // real provider, so the TextDelta path is exercised.
                for b in &resp.blocks {
                    if let Block::Text(t) = b {
                        on_event(ChatEvent::TextDelta(t.clone()));
                    }
                }
                Ok(resp)
            }
            MockTurn::Err(msg) => Err(anyhow!(msg)),
        }
    }
}

// ── response builders ───────────────────────────────────────────────

/// A normal turn that ends the run with some text.
pub(crate) fn resp_text(text: &str, usage: Option<Usage>) -> MockTurn {
    MockTurn::Ok(ChatResponse {
        blocks: vec![Block::Text(text.into())],
        stop_reason: "end_turn".into(),
        usage,
    })
}

/// A turn requesting one tool call (stop_reason "tool_use"), so the loop
/// continues into tool execution.
pub(crate) fn resp_tool(id: &str, name: &str, input: Value, usage: Option<Usage>) -> MockTurn {
    MockTurn::Ok(ChatResponse {
        blocks: vec![Block::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }],
        stop_reason: "tool_use".into(),
        usage,
    })
}

pub(crate) fn mk_usage(input_tokens: u64, output_tokens: u64) -> Option<Usage> {
    Some(Usage {
        input_tokens,
        output_tokens,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
    })
}

/// Like `mk_usage` but also sets the prompt-cache fields — for tests asserting
/// cache tokens flow through the agent loop honestly.
pub(crate) fn mk_usage_cache(
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
) -> Option<Usage> {
    Some(Usage {
        input_tokens,
        output_tokens,
        cache_creation_input_tokens,
        cache_read_input_tokens,
    })
}
