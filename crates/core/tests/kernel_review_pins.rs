//! Kernel review pins (external adversarial review round, kernel shard):
//! each test pins a contract that a review probe attacked. "EXPECTED"
//! comments state the documented discipline; assertions encode the EXPECTED
//! side so a regression is the failure. Gated on the `mock` feature.
#![cfg(feature = "mock")]

use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tiny_agent::provider::mock::{MockProvider, MockTurn};
use tiny_agent::provider::{ChatRequest, ChatResponse, Provider};
use tiny_agent::tools::ToolCtx;
use tiny_agent::{
    Agent, AgentEvent, Block, Config, Hooks, Message, ToolDecision, TurnStopDecision,
};

fn user_text(s: &str) -> Message {
    Message {
        id: None,
        role: tiny_agent::Role::User,
        content: vec![Block::Text(s.to_string())],
    }
}

fn cfg_with(max_turns: u32) -> Config {
    let mut c = serde_json::from_str::<Config>(
        r#"{"provider":"openai","api_key_env":"MOCK_API_KEY","model":"m","inject_environment":false}"#,
    )
    .expect("valid test config");
    c.limits.max_turns = max_turns;
    c.limits.timeout_secs = 0;
    c.limits.compress_threshold_chars = 0;
    c
}

fn test_ctx() -> ToolCtx {
    ToolCtx {
        tool_absent_notes: Vec::new(),
        workdir: std::env::current_dir().unwrap(),
        max_output_chars: 1000,
        restrict_to_workdir: false,
        cancel: None,
        shell_timeout_secs: 120,
        hooks: None,
        subagent: None,
        budget: None,
        deadline: None,
    }
}

// ── Zero-block finalization window honors a cancel that lands after chat()
// returned ─────────────────────────────────────────────────────────────────
// The non-zero-block path overrides stop_reason to "cancelled" when the flag
// landed between chat() returning and the abort check (pinned M8 test). The
// zero-block exit (empty assistant turn) must honor it too: the window
// includes two host event callbacks (Usage fires via bill_chat before the
// is_empty check), so a host that cancels from its event callback (token
// meter) — or a Ctrl-C that lands there — must not be misreported as
// end_turn / max_tokens.

/// Provider: one response with ZERO blocks, non-zero usage, caller-chosen
/// stop_reason.
struct ZeroBlockProvider {
    stop_reason: &'static str,
}
impl Provider for ZeroBlockProvider {
    fn chat(
        &self,
        _req: &ChatRequest,
        _on_event: &mut dyn FnMut(tiny_agent::provider::ChatEvent),
    ) -> anyhow::Result<ChatResponse> {
        Ok(ChatResponse {
            blocks: vec![],
            stop_reason: self.stop_reason.into(),
            usage: Some(tiny_agent::provider::Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            }),
        })
    }
}

fn run_zero_block(stop_reason: &'static str) -> String {
    let flag = Arc::new(AtomicBool::new(false));
    let f2 = flag.clone();
    let mut agent = Agent::new(
        &cfg_with(5),
        Box::new(ZeroBlockProvider { stop_reason }),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
    agent.set_cancel(flag.clone());
    let o = agent
        .run(&mut |ev: AgentEvent| {
            if matches!(ev, AgentEvent::Usage { .. }) {
                // Host cancels from its event callback (the Usage event fires
                // BEFORE the zero-block exit is decided).
                f2.store(true, Ordering::Relaxed);
            }
        })
        .expect("run should end Ok");
    o.stop_reason
}

#[test]
fn zero_block_exit_honors_post_chat_cancel_end_turn() {
    assert_eq!(
        run_zero_block("end_turn"),
        "cancelled",
        "zero-block exit must honor a cancel that landed in the post-chat window"
    );
}

#[test]
fn zero_block_exit_honors_post_chat_cancel_max_tokens() {
    // Same window with a max_tokens zero-block (salvage is correctly skipped
    // by its !cancelled() guard, but the break must still report the cancel).
    assert_eq!(run_zero_block("max_tokens"), "cancelled");
}

// ── Control: the pinned non-zero-block override still works ───────────────
#[test]
fn text_block_cancel_race_is_cancelled() {
    let flag = Arc::new(AtomicBool::new(false));
    let f2 = flag.clone();
    let (prov, _state) = MockProvider::new(vec![MockTurn::Ok(ChatResponse {
        blocks: vec![Block::Text("partial".into())],
        stop_reason: "end_turn".into(),
        usage: Some(tiny_agent::provider::Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }),
    })]);
    let mut agent = Agent::new(
        &cfg_with(5),
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
    agent.set_cancel(flag);
    let o = agent
        .run(&mut |ev: AgentEvent| {
            if matches!(ev, AgentEvent::Usage { .. }) {
                f2.store(true, Ordering::Relaxed);
            }
        })
        .unwrap();
    assert_eq!(o.stop_reason, "cancelled", "control: pinned M8 behavior");
}

// ── Cancel landing during on_turn_stopping + Stop decision ────────────────
// The arbitration hook can deliberate unboundedly (approval-style). A cancel
// landing during the hook must surface even when the hook returns Stop — the
// run may not report "end_turn" (exit 0) for a run the user already aborted.
// (The Continue twin is covered by the loop-top check:
// turn_arbitration_cancel_mid_continuation_reports_cancelled.)

struct CancelDuringHook {
    flag: Arc<AtomicBool>,
    /// What the hook returns after arming the cancel.
    stop: bool,
}
impl Hooks for CancelDuringHook {
    fn before_tool(&self, _: &str, _: &serde_json::Value, _: &ToolCtx) -> ToolDecision {
        ToolDecision::Allow
    }
    fn on_turn_stopping(&self, _info: &tiny_agent::TurnStopInfo) -> TurnStopDecision {
        self.flag.store(true, Ordering::Relaxed);
        if self.stop {
            TurnStopDecision::Stop
        } else {
            TurnStopDecision::Continue {
                note: "keep going".into(),
            }
        }
    }
}

#[test]
fn cancel_during_arbitration_hook_with_stop_decision_is_cancelled() {
    let flag = Arc::new(AtomicBool::new(false));
    let (prov, _s) = MockProvider::new(vec![mock::resp_text("all done", None)]);
    let mut agent = Agent::new(
        &cfg_with(5),
        Box::new(prov),
        vec![],
        vec![user_text("task")],
        test_ctx(),
    );
    agent.set_cancel(flag.clone());
    agent.set_hooks(Arc::new(CancelDuringHook { flag, stop: true }));
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(
        o.stop_reason, "cancelled",
        "cancel landing during the hook must surface even on a Stop decision"
    );
}

#[test]
fn control_cancel_during_arbitration_hook_with_continue_decision_is_cancelled() {
    // Control: the Continue twin — loop-top check catches the cancel.
    let flag = Arc::new(AtomicBool::new(false));
    let (prov, _s) = MockProvider::new(vec![
        mock::resp_text("claim 1", None),
        mock::resp_text("claim 2", None),
    ]);
    let mut agent = Agent::new(
        &cfg_with(5),
        Box::new(prov),
        vec![],
        vec![user_text("task")],
        test_ctx(),
    );
    agent.set_cancel(flag.clone());
    agent.set_hooks(Arc::new(CancelDuringHook { flag, stop: false }));
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(
        o.stop_reason, "cancelled",
        "control: pinned Continue-twin behavior"
    );
}

// ── Consecutive Continue decisions up to and past the cap ─────────────────
// 8 end_turn turns, always-continue hook: decisions 1..=6 accepted, decision
// 7 downgraded up front (arbitration_capped), the 8th end_turn never happens
// because the run ended on turn 7's model reply. Notes: 6 delivered
// (deferred=false), the 7th survives only in the event.

struct AlwaysContinue;
impl Hooks for AlwaysContinue {
    fn on_turn_stopping(&self, _info: &tiny_agent::TurnStopInfo) -> TurnStopDecision {
        TurnStopDecision::Continue {
            note: "not done yet".into(),
        }
    }
}

#[test]
fn six_continues_then_cap_bookkeeping() {
    let turns: Vec<MockTurn> = (0..8)
        .map(|i| mock::resp_text(&format!("claim {i}"), None))
        .collect();
    let (prov, _s) = MockProvider::new(turns);
    let mut agent = Agent::new(
        &cfg_with(30),
        Box::new(prov),
        vec![],
        vec![user_text("task")],
        test_ctx(),
    );
    agent.set_hooks(Arc::new(AlwaysContinue));
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "arbitration_capped");
    assert_eq!(
        o.turns, 7,
        "6 continues accepted → turns 1..7 run, cap refuses an 8th start"
    );
    let arb: Vec<(u32, bool)> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TurnArbitration {
                turn, continued, ..
            } => Some((*turn, *continued)),
            _ => None,
        })
        .collect();
    assert_eq!(arb.len(), 7, "one event per non-default decision");
    assert!(arb[..6].iter().all(|(_, c)| *c), "first six accepted");
    assert_eq!(arb[6], (7, false), "seventh downgraded at the cap");
    let delivered = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::NoteInjected {
                    deferred: false,
                    ..
                }
            )
        })
        .count();
    assert_eq!(delivered, 6, "six notes delivered at turn tops");
    let deferred = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::NoteInjected { deferred: true, .. }))
        .count();
    assert_eq!(
        deferred, 0,
        "the capped note is never queued, nothing to flush"
    );
    // Turn timings: one row per started turn.
    assert_eq!(o.turn_timings.len(), o.turns as usize);
}

// ── stuck_loop never arbitrates ───────────────────────────────────────────
// Six identical tool calls (one per turn) trip STUCK_STOP_AT at the turn
// tail; an always-continue hook must never fire for them.

#[test]
fn stuck_loop_never_consults_arbitration() {
    let turns: Vec<MockTurn> = (0..6)
        .map(|i| mock::resp_tool(&format!("t{i}"), "echo", json!({"q": "same"}), None))
        .collect();
    let (prov, _s) = MockProvider::new(turns);
    let mut agent = Agent::new(
        &cfg_with(30),
        Box::new(prov),
        vec![Box::new(EchoToolTool)],
        vec![user_text("task")],
        test_ctx(),
    );
    agent.set_hooks(Arc::new(AlwaysContinue));
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "stuck_loop");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnArbitration { .. })),
        "stuck_loop is a forced termination — no arbitration"
    );
    assert_eq!(o.turns, 6);
}

struct EchoToolTool;
impl tiny_agent::tools::Tool for EchoToolTool {
    fn def(&self) -> tiny_agent::provider::ToolDef {
        tiny_agent::provider::ToolDef {
            name: "echo".into(),
            description: "review pin echo".into(),
            schema: json!({"type": "object"}),
        }
    }
    fn run(
        &self,
        _input: &serde_json::Value,
        _ctx: &ToolCtx,
    ) -> anyhow::Result<tiny_agent::tools::ToolOutput> {
        Ok(tiny_agent::tools::ToolOutput::text("same result"))
    }
}

// ── Error-then-rerun state reuse ──────────────────────────────────────────
// Run 1: turn 1 end_turn ok (usage billed), turn 2 provider Err → RunError.
// Run 2 on the SAME agent: fresh usage totals, history intact (assistant from
// turn 1 kept, N:N pairing fine), ids never reused, run ends end_turn.

#[test]
fn error_then_rerun_on_same_agent_reuses_state_cleanly() {
    let (prov, state) = MockProvider::new(vec![
        // Turn 1 requests a tool (so the loop continues), then turn 2's chat
        // request fails → RunError after a billed turn.
        mock::resp_tool("t1", "echo", json!({"q": "x"}), mock::mk_usage(100, 10)),
        MockTurn::Err("provider blew up".into()),
        mock::resp_text("turn two after rerun", mock::mk_usage(7, 3)),
    ]);
    let mut agent = Agent::new(
        &cfg_with(5),
        Box::new(prov),
        vec![],
        vec![user_text("task")],
        test_ctx(),
    );
    let mut ev1 = Vec::new();
    let err = match agent.run(&mut |e| ev1.push(e)) {
        Err(e) => e,
        Ok(_) => panic!("expected run-1 failure"),
    };
    assert_eq!(err.usage.input_tokens, 100, "run-1 usage snapshot");
    assert_eq!(err.turns, 2, "the failed turn counts");
    // No Done on the error path.
    assert!(!ev1.iter().any(|e| matches!(e, AgentEvent::Done { .. })));

    let o2 = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o2.stop_reason, "end_turn");
    assert_eq!(o2.turns, 1, "fresh run: only the retried turn");
    assert_eq!(
        o2.input_tokens, 7,
        "usage reset per run — no run-1 carryover"
    );
    // History after run 1: [user, assistant(tool_use), user(tool_result)] —
    // the failed turn's chat pushed nothing.
    let cap = state.captured();
    assert_eq!(
        cap[1].message_count, 3,
        "failed turn's request saw the tool-result user msg"
    );
    assert_eq!(
        cap[2].message_count, 3,
        "rerun request carries the same history"
    );
    // Ids unique across both runs.
    let mut ids: Vec<String> = agent
        .messages()
        .iter()
        .filter_map(|m| m.id.clone())
        .collect();
    ids.sort();
    let n = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), n, "no duplicate message ids after error+rerun");
}

// Re-export the mock helper module used above.
use tiny_agent::provider::mock;

// ── Deferred flush lands before Done, and the flushed note is model-visible
// on the next run's first request ──────────────────────────────────────────
// Budget guard ends the run at the loop top right after a Continue was
// accepted; the queued note flushes deferred BEFORE Done; the next run's
// first request carries the marked block.

struct ContinueOnce {
    remaining: AtomicBool,
}
impl Hooks for ContinueOnce {
    fn on_turn_stopping(&self, _info: &tiny_agent::TurnStopInfo) -> TurnStopDecision {
        if self.remaining.swap(false, Ordering::Relaxed) {
            TurnStopDecision::Continue {
                note: "verify the failing test".into(),
            }
        } else {
            TurnStopDecision::Stop
        }
    }
}

#[test]
fn deferred_flush_order_and_next_run_visibility() {
    // max_total_tokens=1: turn 1 bills its chat (usage 10); the gate fires
    // only at the loop top AFTER the first turn, so: turn 1 runs end_turn,
    // arbitration Continue accepted (note queued), loop top sees spend(10) >=
    // cap(1) → budget_exceeded. Note flushes deferred.
    let mut cfg = cfg_with(10);
    cfg.limits.max_total_tokens = 1;
    let (prov, state) = MockProvider::new(vec![
        mock::resp_text("first attempt", mock::mk_usage(10, 1)),
        // Next run's first request.
        mock::resp_text("second run final", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("task")],
        test_ctx(),
    );
    agent.set_hooks(Arc::new(ContinueOnce {
        remaining: AtomicBool::new(true),
    }));
    let mut events = Vec::new();
    let o1 = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o1.stop_reason, "budget_exceeded");
    let note_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::NoteInjected { deferred: true, .. }))
        .expect("deferred flush happened");
    let done_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("done emitted");
    assert!(note_pos < done_pos, "flush BEFORE Done");
    // Next run: the note is in the first request's user texts.
    let o2 = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o2.stop_reason, "end_turn");
    let cap = state.captured();
    let req = cap.last().unwrap();
    assert!(
        req.user_texts
            .iter()
            .any(|t| t.contains("verify the failing test")),
        "deferred note is model-visible on the next run's first request"
    );
}

// ── summary_call does NOT retry deterministic failures ────────────────────
// The retry guard applies RunError's transient/deterministic taxonomy to the
// bare provider error: a 401 on the summary gets no backoff + full-prompt
// resend (the compression is disabled for the run — compress_failed — but no
// second upload of a possibly multi-MB summary prompt).

#[test]
fn summary_retry_skips_deterministic_401() {
    let mut cfg = cfg_with(5);
    cfg.limits.compress_threshold_chars = 30;
    cfg.limits.keep_recent_messages = 2;
    let long = "x".repeat(40);
    let bulk = "y".repeat(2000);
    let initial = vec![
        user_text(&long),
        Message::assistant(vec![Block::Text(bulk)]),
        user_text("u1"),
        Message::assistant(vec![Block::Text("a2".into())]),
        user_text("u2"),
    ];
    let (prov, state) = MockProvider::new(vec![
        // A REAL 401 shape: ApiStatusError in the chain (what the openai
        // provider produces), not a bare string — the transient ladder
        // classifies on the structured status.
        MockTurn::ErrAny(
            tiny_agent::provider::ApiStatusError::new(
                401,
                "OpenAI-compatible API error 401: invalid api key".into(),
            )
            .into(),
        ), // summary attempt
        mock::resp_text("final answer", None), // main call
    ]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    let calls = state.calls();
    assert_eq!(
        calls, 2,
        "a deterministic 401 on the summary must not be retried"
    );
}

// ── Assistant-led head survives the fold in the session archive ───────────
// A user-led head keeps its original blocks live; an assistant-led head
// (supported shape per fold_summary docs) is replaced by a FRESH empty user
// head — so compress() must archive the original head, or it vanishes from
// both live history and the session file (the summary gist is not the
// record).

#[test]
fn assistant_led_head_is_archived_on_fold() {
    use tiny_agent::session;
    let mut cfg = cfg_with(5);
    cfg.limits.compress_threshold_chars = 30;
    cfg.limits.keep_recent_messages = 2;
    let head_marker = "ASSISTANT_HEAD_ORIGINAL_9f31";
    let initial = vec![
        Message::assistant(vec![Block::Text(format!(
            "{head_marker} bulk {}",
            "y".repeat(2000)
        ))]),
        user_text("u1"),
        Message::assistant(vec![Block::Text("a2".into())]),
        user_text("u2"),
        Message::assistant(vec![Block::Text("a3".into())]),
    ];
    let (prov, _state) = MockProvider::new(vec![
        mock::resp_text("SUMMARY", None),
        mock::resp_text("final answer", None),
    ]);
    let mut folded_capture = Vec::new();
    let dir = std::env::temp_dir().join(format!("ta-pin-arch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.json");
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    agent.enable_session(path.clone());
    let o = agent
        .run(&mut |e| {
            if let AgentEvent::Compressed {
                folded_ids,
                dropped,
                ..
            } = e
            {
                folded_capture.push((dropped, folded_ids));
            }
        })
        .unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    // The head counts as folded for this shape: dropped/folded_ids cover the
    // archived range [0..cut] (head + middle), not just the middle.
    assert_eq!(folded_capture.len(), 1, "exactly one fold");
    let (dropped, folded_ids) = &folded_capture[0];
    assert_eq!(
        *dropped, 4,
        "assistant head + middle (4 messages left live history)"
    );
    assert_eq!(
        folded_ids.len(),
        4,
        "folded_ids cover every archived message"
    );
    assert!(
        folded_ids.iter().all(|id| id.starts_with('m')),
        "ids are the minted message ids"
    );
    let live_has_head = agent.messages().iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, Block::Text(t) if t.contains(head_marker)))
    });
    let (_m, loaded, archive) = session::load_with_archive(&path).unwrap().unwrap();
    let archive_has_head = archive.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, Block::Text(t) if t.contains(head_marker)))
    });
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        archive_has_head,
        "the original assistant head must be in the session archive"
    );
    assert!(
        !live_has_head,
        "the assistant-led head is replaced by the fresh user head in live history"
    );
    let _ = loaded;
}

// ── set_tool_absent_notes: the host-facing absence-note hot setter ────────
// Public API with zero prior coverage (review finding): the setter must feed
// the dispatch path — an absent tool call reports the host's note instead of
// the bare "unknown tool" — and a later call must REPLACE the notes
// (hot-sync semantics, mirroring the build-time config list).

#[test]
fn set_tool_absent_notes_feeds_dispatch_and_replaces() {
    let (prov, _s) = MockProvider::new(vec![
        mock::resp_tool("t1", "shell", json!({"cmd": "ls"}), None),
        mock::resp_tool("t2", "run_code", json!({"code": "1"}), None),
        mock::resp_text("done", None),
    ]);
    let mut agent = Agent::new(
        &cfg_with(5),
        Box::new(prov),
        vec![], // no tools installed: every call is "absent"
        vec![user_text("task")],
        test_ctx(),
    );
    agent.set_tool_absent_notes(vec![(
        "shell".into(),
        "shell is disabled on this deployment".into(),
    )]);
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    let result_text = |tool_use_id: &str| -> String {
        agent
            .messages()
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                Block::ToolResult {
                    tool_use_id: id,
                    content,
                    ..
                } if id == tool_use_id => Some(content.clone()),
                _ => None,
            })
            .next()
            .unwrap_or_default()
    };
    assert_eq!(
        result_text("t1"),
        "shell is disabled on this deployment",
        "the host note replaces the bare unknown-tool error"
    );
    assert!(
        result_text("t2").contains("unknown tool"),
        "unlisted absent tools keep the bare error: {:?}",
        result_text("t2")
    );
}
