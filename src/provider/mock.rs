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
pub(crate) struct Captured {
    pub(crate) message_count: usize,
    pub(crate) last_role: Option<Role>,
    /// Role of every message in the request, in order — lets tests assert no
    /// two adjacent messages share a role (Anthropic rejects that with a 400).
    pub(crate) roles: Vec<Role>,
    /// tool_use blocks on the trailing assistant message.
    pub(crate) trailing_tool_uses: usize,
    /// tool_result blocks on the trailing user message.
    pub(crate) trailing_tool_results: usize,
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
        self.captured
            .lock()
            .unwrap()
            .iter()
            .map(|c| Captured {
                message_count: c.message_count,
                last_role: c.last_role,
                roles: c.roles.clone(),
                trailing_tool_uses: c.trailing_tool_uses,
                trailing_tool_results: c.trailing_tool_results,
            })
            .collect()
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
        self.state.captured.lock().unwrap().push(Captured {
            message_count: req.messages.len(),
            last_role,
            roles: req.messages.iter().map(|m| m.role).collect(),
            trailing_tool_uses,
            trailing_tool_results,
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
    })
}
