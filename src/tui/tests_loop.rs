//! Loop-level tests: worker messages and keys driven through a `Session` the
//! way `ui_loop` drives it — no terminal, real channels. The unit tests in
//! `tests` pin the pieces (the command grammar, the splitters, the renderers);
//! these pin what happens BETWEEN them: which rows a run leaves behind, what
//! the queue does when a run ends, what a key does in each run phase, and what
//! the viewport makes of it.

use super::keys::Effect;
use super::session::{RunPhase, Session, Startup};
use super::transcript::{Row, SysLevel};
use super::view;
use super::worker::{CompactReport, Input, UiMsg};
use crate::agent::{AgentEvent, CompressTrigger, TodoItem};
use crate::config::Config;
use crate::context_report as cr;
use crate::provider::Usage;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::layout::Position;
use ratatui::Terminal;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use tiny_agent::diag::DiagLevel;

// ── Harness ───────────────────────────────────────────────────────────

/// Body width the loop hands the reducer on a 100-column terminal.
const SPLIT_W: usize = 82;

/// The one row every fresh session starts with.
const HELLO: &str = "!session ready · test-model · 0 tools";

/// The size of the cell grid every `screen` is painted into.
const W: u16 = 80;
const H: u16 = 8;

fn cfg() -> Config {
    serde_json::from_value(json!({
        "provider": "openai",
        "api_key_env": "K",
        "model": "test-model",
    }))
    .unwrap()
}

fn session() -> (Session, mpsc::Receiver<Option<Input>>, Arc<AtomicBool>) {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    (Session::for_test(tx, cancel.clone()), rx, cancel)
}

/// One agent event, delivered the way the loop delivers worker messages.
fn ev(s: &mut Session, event: AgentEvent) {
    s.on_msg(UiMsg::Event(event), &cfg(), SPLIT_W);
}

/// One event from a named sub-agent.
fn sub(s: &mut Session, name: &str, event: AgentEvent) {
    ev(
        s,
        AgentEvent::SubAgent {
            name: name.into(),
            event: Box::new(event),
        },
    );
}

fn msg(s: &mut Session, msg: UiMsg) {
    s.on_msg(msg, &cfg(), SPLIT_W);
}

/// The repaint step that keeps the popup in step with the input text.
fn refresh(s: &mut Session) {
    s.composer
        .popup
        .update_for(&s.composer.input.text(), s.run.phase.busy());
}

/// Press a key the way the loop does: handle it, then run the repaint step.
/// The loop always repaints before the next key arrives, so a test must too —
/// a command that emptied the composer leaves the popup open until the next
/// repaint, and the stale open popup would swallow the next key here and
/// never there.
fn key(s: &mut Session, code: KeyCode, mods: KeyModifiers) -> Effect {
    let effect = s.on_key(KeyEvent::new(code, mods), &cfg());
    refresh(s);
    effect
}

fn enter(s: &mut Session) -> Effect {
    key(s, KeyCode::Enter, KeyModifiers::NONE)
}

fn esc(s: &mut Session) -> Effect {
    key(s, KeyCode::Esc, KeyModifiers::NONE)
}

fn ctrl(s: &mut Session, c: char) -> Effect {
    key(s, KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// Type character by character, the way the loop reads keys (appends).
fn typed(s: &mut Session, text: &str) {
    for ch in text.chars() {
        key(s, KeyCode::Char(ch), KeyModifiers::NONE);
    }
}

/// Put a line in the composer and press Enter on it.
///
/// An argument-taking command (`/out`, `/call`, `/thinking`, `/export`,
/// `/compact`) is spelled with its trailing space — what completion leaves
/// behind: on the bare name the popup's Enter only completes.
fn command(s: &mut Session, text: &str) -> Effect {
    s.composer.input.set_text(text);
    // The loop repaints after any text change (history recall and a refused
    // command put text back the same way), and the repaint is what re-opens
    // or closes the popup for the new text.
    refresh(s);
    enter(s)
}

/// Everything the worker was sent, in order (`None` = the quit signal).
fn drain(rx: &mpsc::Receiver<Option<Input>>) -> Vec<Option<Input>> {
    let mut out = Vec::new();
    while let Ok(x) = rx.try_recv() {
        out.push(x);
    }
    out
}

fn text_msg(text: &str) -> Option<Input> {
    Some(Input::Text(text.into()))
}

/// A run in flight: "go" was sent and the agent's first turn began. The
/// channel is left EMPTY, so a later `drain` lists only what the test's own
/// steps sent.
fn in_flight(s: &mut Session, rx: &mpsc::Receiver<Option<Input>>) {
    assert_eq!(command(s, "go"), Effect::None);
    assert_eq!(drain(rx), vec![text_msg("go")]);
    ev(s, AgentEvent::TurnStart(1));
    assert_eq!(s.run.phase, RunPhase::Running);
}

/// A failed run that left `msgs` held in a paused queue.
fn failed_with_queue(s: &mut Session, rx: &mpsc::Receiver<Option<Input>>, msgs: &[&str]) {
    in_flight(s, rx);
    for m in msgs {
        assert_eq!(command(s, m), Effect::None);
    }
    msg(s, UiMsg::Error("boom".into()));
    assert_eq!(s.run.phase, RunPhase::Failed);
    assert!(s.queue_paused);
}

fn tool_call(name: &str, input: serde_json::Value) -> AgentEvent {
    AgentEvent::ToolCall {
        name: name.into(),
        input,
    }
}

fn tool_result(name: &str, content: &str) -> AgentEvent {
    AgentEvent::ToolResult {
        name: name.into(),
        is_error: false,
        content: content.into(),
    }
}

fn todo(subject: &str, status: &str) -> TodoItem {
    TodoItem {
        subject: subject.into(),
        status: status.into(),
    }
}

fn usage(
    in_tokens: u64,
    out: u64,
    cache_create: u64,
    cache_read: u64,
    tool_folded: bool,
) -> AgentEvent {
    AgentEvent::Usage {
        input_tokens: in_tokens,
        output_tokens: out,
        cache_creation_input_tokens: cache_create,
        cache_read_input_tokens: cache_read,
        tool_folded,
        reasoning_tokens: 0,
    }
}

fn bill(input_tokens: u64, output_tokens: u64) -> Usage {
    Usage {
        input_tokens,
        output_tokens,
        ..Usage::default()
    }
}

fn breakdown(total: u64) -> cr::ContextBreakdown {
    cr::ContextBreakdown {
        system: cr::SystemBreakdown::default(),
        tools: cr::ToolsBreakdown::default(),
        messages: cr::MessagesBreakdown {
            tokens: total,
            by_kind: cr::MessageKinds::default(),
        },
        total_tokens: total,
        calibration: None,
    }
}

fn compact_done(folded: bool, aborted: bool, usage: Option<Usage>) -> UiMsg {
    UiMsg::CompactDone(CompactReport {
        folded,
        aborted,
        note: None,
        usage,
    })
}

// ── Reading the session back ──────────────────────────────────────────

/// One short token per row, in transcript order — the shape of the
/// conversation: `U:` user, `A:` assistant (its text so far), `T:` reasoning,
/// `C:` tool call, `R:` tool result (`!` = failed, `+` = opened), `!` a notice.
fn shape(s: &Session) -> Vec<String> {
    s.rows
        .iter()
        .map(|r| match r {
            Row::User(t) => format!("U:{t}"),
            Row::Assistant(t) | Row::AssistantCont { text: t, .. } => format!("A:{t}"),
            Row::Thinking { text, .. } | Row::ThinkingFull(text) => format!("T:{text}"),
            Row::ToolCall { name, .. } => format!("C:{name}"),
            Row::ToolResult {
                name,
                is_error,
                expanded,
                ..
            } => format!(
                "R:{name}{}",
                match (is_error, expanded) {
                    (true, _) => "!",
                    (false, true) => "+",
                    (false, false) => "",
                }
            ),
            Row::System { text, .. } => format!("!{text}"),
        })
        .collect()
}

/// Every notice row since the session started, in order.
fn notices(s: &Session) -> Vec<String> {
    s.rows
        .iter()
        .filter_map(|r| match r {
            Row::System { text, .. } if !text.starts_with("session ready") => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn latest_notice(s: &Session) -> String {
    notices(s).pop().unwrap_or_else(|| {
        panic!("no notice row; rows are {:#?}", shape(s));
    })
}

fn levels(s: &Session) -> Vec<SysLevel> {
    s.rows
        .iter()
        .filter_map(|r| match r {
            Row::System { text, level } if !text.starts_with("session ready") => Some(*level),
            _ => None,
        })
        .collect()
}

fn assistant_rows(s: &Session) -> Vec<&str> {
    s.rows
        .iter()
        .filter_map(|r| match r {
            Row::Assistant(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

fn thinking_rows(s: &Session) -> Vec<&str> {
    s.rows
        .iter()
        .filter_map(|r| match r {
            Row::Thinking { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_names(s: &Session, results: bool) -> Vec<&str> {
    s.rows
        .iter()
        .filter_map(|r| match r {
            Row::ToolCall { name, .. } if !results => Some(name.as_str()),
            Row::ToolResult { name, .. } if results => Some(name.as_str()),
            _ => None,
        })
        .collect()
}

/// Tools currently executing, in the order their calls arrived.
fn running(s: &Session) -> Vec<&str> {
    s.run
        .tool_running
        .calls
        .iter()
        .map(|(n, _)| n.as_str())
        .collect()
}

fn queued(s: &Session) -> Vec<String> {
    s.queued.iter().cloned().collect()
}

fn history(s: &Session) -> Vec<String> {
    s.composer.history.clone()
}

fn totals(s: &Session) -> [u64; 4] {
    [
        s.usage.in_tokens,
        s.usage.out_tokens,
        s.usage.cache_create,
        s.usage.cache_read,
    ]
}

/// `Row` carries `Instant`s and has no `PartialEq`: compare by what the row
/// IS for the reader (kind, name, text, open/failure flags).
fn same_rows(a: &[Row], b: &[Row]) -> bool {
    let describe = |rows: &[Row]| -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                Row::ToolCall {
                    name,
                    input,
                    collapsed,
                    expanded,
                    ..
                } => format!("C:{name}:{input}:{collapsed}:{expanded}"),
                Row::ToolResult {
                    name,
                    content,
                    collapsed,
                    is_error,
                    expanded,
                    ..
                } => format!("R:{name}:{content}:{collapsed}:{is_error}:{expanded}"),
                Row::System { text, level } => format!("S:{level:?}:{text}"),
                Row::Thinking { text, tight } => format!("T:{text}:{tight}"),
                Row::AssistantCont { text, tight } => format!("Ac:{text}:{tight}"),
                Row::ThinkingFull(text) => format!("Tf:{text}"),
                Row::User(text) => format!("U:{text}"),
                Row::Assistant(text) => format!("A:{text}"),
            })
            .collect()
    };
    describe(a) == describe(b)
}

// ── A run, event by event ─────────────────────────────────────────────

#[test]
fn a_turn_streams_its_trace_and_its_answer_into_separate_rows() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    // TurnStart: the "thinking" placeholder, nothing streaming yet.
    assert!(s.run.thinking);
    assert_eq!(s.run.activity, "thinking");
    assert!(!s.run.stream_open());

    ev(&mut s, AgentEvent::ThinkingDelta("let me ".into()));
    ev(&mut s, AgentEvent::ThinkingDelta("think".into()));
    assert_eq!(shape(&s), [HELLO, "U:go", "T:let me think"]);
    assert!(s.run.stream_open());

    // The answer opens its OWN row; the trace is final from here on.
    ev(&mut s, AgentEvent::TextDelta("Hello".into()));
    ev(&mut s, AgentEvent::TextDelta(", world".into()));
    assert_eq!(
        shape(&s),
        [HELLO, "U:go", "T:let me think", "A:Hello, world"]
    );
    assert!(!s.run.thinking);
    assert_eq!(s.run.activity, "writing");

    ev(
        &mut s,
        AgentEvent::Done {
            stop_reason: "end_turn".into(),
        },
    );
    assert!(!s.run.stream_open());
    assert!(notices(&s).is_empty(), "a normal turn end is silent");
    assert_eq!(s.run.phase, RunPhase::Running, "…until the worker says so");

    msg(&mut s, UiMsg::RunFinished);
    assert_eq!(s.run.phase, RunPhase::Idle);
    assert!(s.run.run_start.is_none());
    assert_eq!(s.run.activity, "");
}

#[test]
fn a_tool_call_shows_what_runs_and_then_how_long_it_took() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);

    ev(
        &mut s,
        AgentEvent::ToolInputDelta {
            id: "c1".into(),
            name: "read_file".into(),
            delta: "{".into(),
        },
    );
    assert_eq!(s.run.activity, "→ read_file · receiving args");
    assert_eq!(
        s.run.tool_building.as_ref().map(|(n, _)| n.as_str()),
        Some("read_file")
    );
    assert!(
        !s.run.thinking,
        "the placeholder gives way to real activity"
    );
    assert!(!s.run.stream_open());
    assert_eq!(
        shape(&s),
        [HELLO, "U:go"],
        "argument fragments never become rows"
    );

    ev(
        &mut s,
        tool_call("read_file", json!({"path": "src/main.rs"})),
    );
    assert!(s.run.tool_building.is_none());
    assert_eq!(s.run.activity, "→ read_file");
    assert_eq!(running(&s), ["read_file"]);
    assert_eq!(tool_names(&s, false), ["read_file"]);
    assert!(matches!(
        s.rows.last(),
        Some(Row::ToolCall {
            expanded: false,
            ..
        })
    ));

    ev(&mut s, tool_result("read_file", "fn main() {}"));
    assert!(running(&s).is_empty(), "the call is no longer in flight");
    assert_eq!(tool_names(&s, true), ["read_file"]);
    match s.rows.last() {
        Some(Row::ToolResult {
            is_error,
            expanded,
            elapsed,
            content,
            ..
        }) => {
            assert!(!is_error);
            assert!(!expanded, "a success stays collapsed");
            assert_eq!(
                content, "fn main() {}",
                "the result is kept raw for the renderer"
            );
            assert!(elapsed.is_some(), "the header carries the call's duration");
        }
        other => panic!("expected a tool result, got {other:?}"),
    }

    // The collapsed row summarizes arguments as `key=value` — not the raw
    // JSON — and keeps the whole argument for `/call`.
    ev(
        &mut s,
        tool_call(
            "write_file",
            json!({"path": "notes.md", "content": "first\nsecond"}),
        ),
    );
    match s.rows.last() {
        Some(Row::ToolCall {
            collapsed, input, ..
        }) => {
            assert_eq!(collapsed, r#"path=notes.md content="first\nsecond""#);
            assert_eq!(
                *input,
                json!({"path": "notes.md", "content": "first\nsecond"})
            );
        }
        other => panic!("expected a tool call, got {other:?}"),
    }
}

#[test]
fn a_failed_call_opens_itself_only_when_its_output_is_short() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    ev(
        &mut s,
        tool_call("read_file", json!({"path": "missing.txt"})),
    );
    ev(
        &mut s,
        AgentEvent::ToolResult {
            name: "read_file".into(),
            is_error: true,
            content: "No such file or directory (os error 2)".into(),
        },
    );
    // A long failure would flood the scrollback: it stays folded, `/out` opens it.
    ev(&mut s, tool_call("shell", json!({"command": "make test"})));
    ev(
        &mut s,
        AgentEvent::ToolResult {
            name: "shell".into(),
            is_error: true,
            content: "line\n".repeat(40),
        },
    );
    assert_eq!(
        shape(&s),
        [
            HELLO,
            "U:go",
            "C:read_file",
            "R:read_file!",
            "C:shell",
            "R:shell!",
        ]
    );
    let opened: Vec<bool> = s
        .rows
        .iter()
        .filter_map(|r| match r {
            Row::ToolResult { expanded, .. } => Some(*expanded),
            _ => None,
        })
        .collect();
    assert_eq!(opened, [true, false]);
}

#[test]
fn parallel_calls_stay_listed_until_each_one_returns() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    for prompt in ["scan the kernel", "scan the bindings"] {
        ev(&mut s, tool_call("task", json!({"prompt": prompt})));
    }
    assert_eq!(running(&s), ["task", "task"]);
    ev(&mut s, tool_result("task", "first back"));
    assert_eq!(running(&s), ["task"]);
    ev(&mut s, tool_result("task", "second back"));
    assert!(running(&s).is_empty());
    assert_eq!(tool_names(&s, true), ["task", "task"]);
}

#[test]
fn only_an_abnormal_stop_leaves_a_notice() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    let done = |reason: &str| AgentEvent::Done {
        stop_reason: reason.into(),
    };
    for normal in [
        "end_turn",
        "tool_use",
        "stop",
        "tool_calls",
        "stop_sequence",
        "cancelled",
    ] {
        ev(&mut s, done(normal));
    }
    assert!(notices(&s).is_empty(), "routine turn ends are silent");

    for abnormal in ["max_tokens", "length", "error", "stuck_loop"] {
        ev(&mut s, done(abnormal));
    }
    assert_eq!(
        notices(&s),
        [
            "[done: max_tokens]",
            "[done: length]",
            "[done: error]",
            "[done: stuck_loop]",
        ]
    );
    assert_eq!(levels(&s), [SysLevel::Warn; 4]);
}

#[test]
fn usage_sums_every_bill_but_only_a_direct_request_measures_the_context() {
    let (mut s, _rx, _cancel) = session();
    ev(&mut s, usage(10, 5, 100, 1000, false));
    assert_eq!(totals(&s), [10, 5, 100, 1000]);
    assert_eq!(s.usage.ctx_tokens, 1115);

    ev(&mut s, usage(12, 6, 0, 1200, false));
    assert_eq!(totals(&s), [22, 11, 100, 2200]);
    assert_eq!(
        s.usage.ctx_tokens, 1218,
        "the latest measurement replaces the last"
    );

    // A sub-agent's folded bill is real cost but no measurement of this history.
    ev(&mut s, usage(500, 300, 0, 0, true));
    assert_eq!(totals(&s), [522, 311, 100, 2200]);
    assert_eq!(s.usage.ctx_tokens, 1218);

    // Hostile numbers saturate instead of wrapping into a small total.
    ev(&mut s, usage(u64::MAX, u64::MAX, u64::MAX, u64::MAX, true));
    assert_eq!(totals(&s), [u64::MAX; 4]);
    assert_eq!(shape(&s), [HELLO], "usage is status-bar state, not a row");
}

#[test]
fn each_bookkeeping_event_leaves_exactly_one_self_describing_row() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    let compressed = |trigger| AgentEvent::Compressed {
        dropped: 12,
        folded_ids: vec![],
        kept: 4,
        trigger,
        estimated_tokens_before: 9000,
        estimated_tokens_after: 2000,
        measured_tokens_before: None,
    };
    let cases = [
        (
            compressed(CompressTrigger::Preflight),
            SysLevel::Info,
            "[context compressed (preflight): dropped 12 messages, 4 kept]",
        ),
        (
            compressed(CompressTrigger::Overflow),
            SysLevel::Info,
            "[context compressed (overflow): dropped 12 messages, 4 kept]",
        ),
        (
            compressed(CompressTrigger::Manual),
            SysLevel::Info,
            "[context compressed (manual): dropped 12 messages, 4 kept]",
        ),
        (
            AgentEvent::Pruned {
                messages: 3,
                chars_freed: 5400,
            },
            SysLevel::Info,
            "[context pruned: 3 messages lost stale tool output, 5400 chars freed]",
        ),
        (
            AgentEvent::Warning("provider retrying".into()),
            SysLevel::Warn,
            "[warn] provider retrying",
        ),
        (
            AgentEvent::NoteInjected {
                kind: "nudge".into(),
                text: "stay on task".into(),
                deferred: false,
            },
            SysLevel::Warn,
            "[supervisor note] stay on task",
        ),
        (
            AgentEvent::NoteInjected {
                kind: "nudge".into(),
                text: "stay on task".into(),
                deferred: true,
            },
            SysLevel::Warn,
            "[supervisor note, deferred to next run] stay on task",
        ),
        (
            AgentEvent::MaxTokensSalvage,
            SysLevel::Warn,
            "[max_tokens hit — one wrap-up turn appended]",
        ),
        (
            AgentEvent::TurnArbitration {
                turn: 3,
                continued: true,
                note: String::new(),
            },
            SysLevel::Warn,
            "[arbitration: completion rejected — one more turn]",
        ),
        (
            AgentEvent::TurnArbitration {
                turn: 9,
                continued: false,
                note: "one more pass".into(),
            },
            SysLevel::Warn,
            "[arbitration capped — note not delivered] one more pass",
        ),
        (
            AgentEvent::CodeLog("step 3 done".into()),
            SysLevel::Info,
            "step 3 done",
        ),
    ];
    for (event, level, text) in cases {
        let before = s.rows.len();
        ev(&mut s, event);
        assert_eq!(s.rows.len(), before + 1, "{text}");
        assert!(
            matches!(
                s.rows.last(),
                Some(Row::System { level: l, text: t }) if *l == level && t == text
            ),
            "{text}"
        );
    }
    assert_eq!(
        s.run.phase,
        RunPhase::Running,
        "bookkeeping does not end the run"
    );
}

#[test]
fn the_plan_is_replaced_wholesale_and_adds_no_row() {
    let (mut s, _rx, _cancel) = session();
    ev(
        &mut s,
        AgentEvent::TodoUpdate {
            todos: vec![todo("read", "completed"), todo("write", "pending")],
        },
    );
    let statuses: Vec<&str> = s.plan.iter().map(|t| t.status.as_str()).collect();
    assert_eq!(statuses, ["completed", "pending"]);
    assert_eq!(
        shape(&s),
        [HELLO],
        "the checklist lives in its tool-result row"
    );

    ev(
        &mut s,
        AgentEvent::TodoUpdate {
            todos: vec![todo("read", "completed")],
        },
    );
    assert_eq!(s.plan.len(), 1, "each update carries the entire plan");
}

#[test]
fn every_worker_message_schedules_a_repaint() {
    let (mut s, rx, _cancel) = session();
    let messages = [
        UiMsg::Event(AgentEvent::TurnStart(1)),
        UiMsg::Event(AgentEvent::TextDelta("x".into())),
        UiMsg::Event(AgentEvent::Done {
            stop_reason: "end_turn".into(),
        }),
        UiMsg::Event(usage(1, 1, 0, 0, false)),
        UiMsg::RunFinished,
        UiMsg::Error("boom".into()),
        UiMsg::Context(breakdown(0)),
        UiMsg::Diag(DiagLevel::Info, "mcp: server ready".into()),
        UiMsg::Diag(DiagLevel::Warn, "[warn] a soft failure".into()),
        compact_done(false, false, None),
    ];
    for m in messages {
        s.dirty = false;
        msg(&mut s, m);
        assert!(s.dirty);
    }
    assert!(
        drain(&rx).is_empty(),
        "messages from the worker send nothing back"
    );
}

// ── How a run ends ────────────────────────────────────────────────────

#[test]
fn a_finished_run_hands_over_to_the_next_queued_message_without_an_idle_gap() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    assert_eq!(command(&mut s, "second"), Effect::None);
    assert_eq!(command(&mut s, "third"), Effect::None);
    assert!(drain(&rx).is_empty(), "nothing reaches a busy worker");
    assert_eq!(queued(&s), ["second", "third"]);

    msg(&mut s, UiMsg::RunFinished);
    assert_eq!(
        s.run.phase,
        RunPhase::Running,
        "no idle gap: Ctrl-C must still cancel, not quit"
    );
    assert!(s.run.run_start.is_some());
    assert!(s.run.thinking);
    assert_eq!(s.run.activity, "thinking");
    assert_eq!(drain(&rx), vec![text_msg("second")]);
    assert_eq!(queued(&s), ["third"]);
    assert_eq!(
        shape(&s),
        [HELLO, "U:go", "U:second"],
        "a queued message becomes a row only when sent"
    );

    msg(&mut s, UiMsg::RunFinished);
    assert_eq!(drain(&rx), vec![text_msg("third")]);
    msg(&mut s, UiMsg::RunFinished);
    assert_eq!(s.run.phase, RunPhase::Idle);
    assert!(queued(&s).is_empty());
    assert!(drain(&rx).is_empty());
}

#[test]
fn a_failed_run_says_why_and_pauses_the_queue_only_when_one_is_held() {
    // Nothing queued: the error alone.
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    ev(&mut s, AgentEvent::TextDelta("half an answer".into()));
    msg(&mut s, UiMsg::Error("socket closed".into()));
    assert_eq!(s.run.phase, RunPhase::Failed);
    assert!(s.run.run_start.is_none());
    assert!(!s.run.stream_open(), "every open stream is closed");
    assert!(!s.queue_paused);
    assert_eq!(notices(&s), ["[error] socket closed"]);
    assert_eq!(levels(&s), [SysLevel::Error]);

    // Messages waiting: they are held, and the user is told how to free them.
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    assert_eq!(command(&mut s, "after"), Effect::None);
    msg(&mut s, UiMsg::Error("socket closed".into()));
    assert_eq!(s.run.phase, RunPhase::Failed);
    assert!(s.queue_paused);
    assert_eq!(queued(&s), ["after"]);
    assert_eq!(
        notices(&s),
        [
            "[error] socket closed",
            "[queue paused — 1 message(s) held; /resume continues, /unqueue edits]",
        ]
    );
    assert_eq!(levels(&s), [SysLevel::Error, SysLevel::Warn]);
    assert!(drain(&rx).is_empty(), "the held message is not sent");
}

#[test]
fn an_error_after_esc_reports_the_error_not_a_cancel() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    assert_eq!(esc(&mut s), Effect::None);
    assert_eq!(s.run.phase, RunPhase::Cancelling);
    msg(&mut s, UiMsg::Error("socket closed".into()));
    assert_eq!(s.run.phase, RunPhase::Failed);
    assert_eq!(
        notices(&s),
        ["[cancelling…]", "[error] socket closed"],
        "the failure is what happened; nothing was cancelled"
    );
}

#[test]
fn the_run_after_a_failure_starts_from_a_clean_slate() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    ev(&mut s, AgentEvent::TextDelta("cut short".into()));
    ev(&mut s, AgentEvent::ThinkingDelta("an unfinished".into()));
    assert!(s.run.stream_open(), "a trace is streaming");
    ev(
        &mut s,
        AgentEvent::ToolInputDelta {
            id: "c1".into(),
            name: "shell".into(),
            delta: "{".into(),
        },
    );
    assert!(s.run.tool_building.is_some());
    sub(
        &mut s,
        "scout",
        AgentEvent::TextDelta("half a bubble".into()),
    );
    assert!(s.run.stream_open(), "a child is mid-bubble");

    msg(&mut s, UiMsg::Error("worker gone".into()));
    assert!(!s.run.stream_open());
    assert!(s.run.tool_building.is_none());
    assert!(running(&s).is_empty());
    assert_eq!(s.run.activity, "");

    // Failed is idle for submission: the next message goes straight out.
    assert_eq!(command(&mut s, "try again"), Effect::None);
    assert_eq!(drain(&rx), vec![text_msg("try again")]);
    assert_eq!(s.run.phase, RunPhase::Running);
    ev(&mut s, AgentEvent::TurnStart(1));
    ev(&mut s, AgentEvent::TextDelta("fresh answer".into()));
    let got = shape(&s);
    assert_eq!(got.last().unwrap(), "A:fresh answer", "{got:?}");
    assert_eq!(got.len(), 8, "{got:?}");
}

#[test]
fn a_held_queue_stays_held_while_a_new_prompt_runs() {
    let (mut s, rx, _cancel) = session();
    failed_with_queue(&mut s, &rx, &["held"]);

    // Failed is idle for submission, but the pause belongs to the held messages.
    assert_eq!(command(&mut s, "fresh"), Effect::None);
    assert_eq!(s.run.phase, RunPhase::Running);
    msg(&mut s, UiMsg::RunFinished);
    assert_eq!(s.run.phase, RunPhase::Idle);
    assert!(s.queue_paused, "the older queue is still explicitly held");
    assert_eq!(queued(&s), ["held"]);
    assert_eq!(drain(&rx), vec![text_msg("fresh")]);
}

// ── Cancel ────────────────────────────────────────────────────────────

#[test]
fn esc_cancels_the_run_and_the_finish_reports_it_once() {
    let (mut s, rx, cancel) = session();
    in_flight(&mut s, &rx);
    ev(&mut s, AgentEvent::TextDelta("partial answer".into()));

    assert_eq!(esc(&mut s), Effect::None);
    assert!(
        cancel.load(Ordering::Relaxed),
        "the worker's cancel flag is raised"
    );
    assert_eq!(s.run.phase, RunPhase::Cancelling);
    assert_eq!(s.run.activity, "cancelling");
    assert!(
        !s.run.stream_open(),
        "the open answer is cut where the cancel found it"
    );
    assert_eq!(notices(&s), ["[cancelling…]"]);

    // A held Esc (key repeat) must not stack notices.
    assert_eq!(esc(&mut s), Effect::None);
    assert_eq!(notices(&s), ["[cancelling…]"]);

    ev(
        &mut s,
        AgentEvent::Done {
            stop_reason: "cancelled".into(),
        },
    );
    msg(&mut s, UiMsg::RunFinished);
    assert_eq!(s.run.phase, RunPhase::Idle);
    assert_eq!(notices(&s), ["[cancelling…]", "[cancelled]"]);
    // The flag stays latched: only the worker clears it, when it accepts the
    // next task (see `worker_main`).
    assert!(cancel.load(Ordering::Relaxed));
    assert!(drain(&rx).is_empty());
}

#[test]
fn cancelling_a_run_drops_the_messages_queued_behind_it() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    assert_eq!(command(&mut s, "later"), Effect::None);

    assert_eq!(esc(&mut s), Effect::None);
    assert!(queued(&s).is_empty());
    assert!(!s.queue_paused, "nothing held, nothing paused");
    assert_eq!(
        notices(&s),
        ["[cancelling…]", "[dropped 1 queued message(s)]"]
    );

    msg(&mut s, UiMsg::RunFinished);
    assert_eq!(s.run.phase, RunPhase::Idle);
    assert_eq!(latest_notice(&s), "[cancelled]");
    assert!(drain(&rx).is_empty());
}

// ── /compact ──────────────────────────────────────────────────────────

#[test]
fn a_compact_is_a_billed_busy_step_that_ends_like_a_run() {
    let (mut s, rx, _cancel) = session();
    s.usage.ctx_tokens = 5_000;

    assert_eq!(command(&mut s, "/compact "), Effect::None);
    assert_eq!(drain(&rx), vec![Some(Input::Compact(None))]);
    assert_eq!(
        s.run.phase,
        RunPhase::Running,
        "Esc must be able to cancel it"
    );
    assert!(s.run.run_start.is_some());
    assert_eq!(s.run.activity, "compacting");
    assert!(!s.run.thinking, "it is no turn: no thinking placeholder");
    assert_eq!(shape(&s), [HELLO], "nothing enters the conversation yet");

    msg(&mut s, compact_done(true, false, Some(bill(1200, 80))));
    assert_eq!(s.run.phase, RunPhase::Idle);
    assert_eq!(
        totals(&s),
        [1200, 80, 0, 0],
        "the summary request is billed"
    );
    assert_eq!(
        s.usage.ctx_tokens, 0,
        "a real fold leaves the old measurement meaningless"
    );
    assert!(
        notices(&s).is_empty(),
        "the fold's own Compressed event is the row"
    );
}

#[test]
fn a_compact_that_folded_nothing_explains_itself_and_keeps_the_context() {
    let (mut s, rx, _cancel) = session();
    s.usage.ctx_tokens = 900;

    msg(
        &mut s,
        UiMsg::CompactDone(CompactReport {
            folded: false,
            aborted: false,
            note: Some((
                DiagLevel::Info,
                "[compact] nothing to fold — the history is too short".into(),
            )),
            usage: None,
        }),
    );
    assert_eq!(s.run.phase, RunPhase::Idle);
    assert_eq!(
        notices(&s),
        ["[compact] nothing to fold — the history is too short"]
    );
    assert_eq!(levels(&s), [SysLevel::Info]);
    assert_eq!(s.usage.ctx_tokens, 900);

    // A failed summary is a warning, and whatever it burned is still billed.
    msg(
        &mut s,
        UiMsg::CompactDone(CompactReport {
            folded: false,
            aborted: false,
            note: Some((DiagLevel::Warn, "[compact] failed: 500".into())),
            usage: Some(bill(300, 0)),
        }),
    );
    assert_eq!(
        notices(&s),
        [
            "[compact] nothing to fold — the history is too short",
            "[compact] failed: 500",
        ]
    );
    assert_eq!(levels(&s), [SysLevel::Info, SysLevel::Warn]);
    assert_eq!(totals(&s), [300, 0, 0, 0]);
    assert_eq!(
        s.usage.ctx_tokens, 900,
        "a history that was not folded is still measured"
    );
    assert!(drain(&rx).is_empty());
}

#[test]
fn an_esc_during_a_compact_is_reported_as_a_cancel_and_nothing_else() {
    let (mut s, _rx, _cancel) = session();
    assert_eq!(command(&mut s, "/compact "), Effect::None);
    assert_eq!(esc(&mut s), Effect::None);
    assert_eq!(s.run.phase, RunPhase::Cancelling);

    msg(&mut s, compact_done(false, true, Some(bill(40, 0))));
    assert_eq!(s.run.phase, RunPhase::Idle);
    assert_eq!(
        notices(&s),
        ["[cancelling…]", "[cancelled]"],
        "no second 'request cancelled' line after it"
    );
    assert_eq!(
        totals(&s),
        [40, 0, 0, 0],
        "what the aborted request burned is billed"
    );
}

#[test]
fn an_esc_that_lost_the_race_to_a_finished_fold_reports_the_fold() {
    let (mut s, _rx, _cancel) = session();
    assert_eq!(command(&mut s, "/compact "), Effect::None);
    assert_eq!(esc(&mut s), Effect::None);
    assert_eq!(s.run.phase, RunPhase::Cancelling);

    msg(&mut s, compact_done(true, false, None));
    assert_eq!(s.run.phase, RunPhase::Idle);
    assert_eq!(
        notices(&s),
        ["[cancelling…]"],
        "nothing was cancelled: no [cancelled]"
    );
}

// ── Kernel notes and /context ─────────────────────────────────────────

#[test]
fn kernel_notes_and_the_context_report_become_rows_without_touching_the_run() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);

    msg(
        &mut s,
        UiMsg::Diag(DiagLevel::Warn, "[warn] MCP stdio read stopped: eof".into()),
    );
    msg(
        &mut s,
        UiMsg::Diag(DiagLevel::Info, "mcp: server ready".into()),
    );
    assert_eq!(
        notices(&s),
        ["[warn] MCP stdio read stopped: eof", "mcp: server ready"]
    );
    assert_eq!(levels(&s), [SysLevel::Warn, SysLevel::Info]);

    msg(&mut s, UiMsg::Context(breakdown(321)));
    let report = latest_notice(&s);
    assert!(report.contains("window: "), "{report}");

    // Read-only: the run in flight is exactly as it was.
    assert_eq!(s.run.phase, RunPhase::Running);
    assert!(s.run.thinking);
    assert_eq!(s.run.activity, "thinking");
    assert_eq!(shape(&s).len(), 5, "welcome, user, three reports");
}

// ── Sub-agents ────────────────────────────────────────────────────────

#[test]
fn a_sub_agent_speaks_in_its_own_tagged_bubble() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    sub(&mut s, "scout", AgentEvent::TextDelta("looking ".into()));
    sub(&mut s, "scout", AgentEvent::TextDelta("around".into()));
    assert_eq!(assistant_rows(&s), ["[scout] looking around"]);
    assert_eq!(s.run.sub_streaming_name.as_deref(), Some("scout"));

    // Another agent — or the parent — never merges into that bubble.
    sub(&mut s, "planner", AgentEvent::TextDelta("plan b".into()));
    ev(&mut s, AgentEvent::TextDelta("parent speaks".into()));
    assert_eq!(
        assistant_rows(&s),
        [
            "[scout] looking around",
            "[planner] plan b",
            "parent speaks"
        ]
    );
    assert!(s.run.sub_streaming_name.is_none());
    assert!(s.run.streaming);

    // Back to the first agent after the interruption: a fresh bubble.
    sub(&mut s, "scout", AgentEvent::TextDelta("done".into()));
    assert_eq!(
        assistant_rows(&s),
        [
            "[scout] looking around",
            "[planner] plan b",
            "parent speaks",
            "[scout] done",
        ]
    );
}

#[test]
fn a_sub_agent_trace_is_tagged_and_closes_its_bubble() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    sub(&mut s, "scout", AgentEvent::TextDelta("half an".into()));
    sub(&mut s, "scout", AgentEvent::ThinkingDelta("wait".into()));
    assert_eq!(
        thinking_rows(&s),
        ["[scout] wait"],
        "the trace says whose it is"
    );
    assert!(
        s.run.sub_streaming_name.is_none(),
        "the trace ends the bubble"
    );

    sub(&mut s, "scout", AgentEvent::TextDelta("answer".into()));
    assert_eq!(assistant_rows(&s), ["[scout] half an", "[scout] answer"]);
}

#[test]
fn sub_agent_tool_rows_are_tagged_and_share_the_parent_tracker() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    ev(&mut s, tool_call("read_file", json!({"path": "a.rs"})));
    sub(
        &mut s,
        "scout",
        tool_call("read_file", json!({"path": "b.rs"})),
    );
    assert_eq!(tool_names(&s, false), ["read_file", "[scout] read_file"]);
    assert_eq!(s.run.activity, "[scout] → read_file");
    assert_eq!(running(&s), ["read_file", "[scout] read_file"]);

    sub(&mut s, "scout", tool_result("read_file", "b"));
    assert_eq!(
        running(&s),
        ["read_file"],
        "the parent's own call is still in flight"
    );
    assert_eq!(tool_names(&s, true), ["[scout] read_file"]);
    assert!(matches!(
        s.rows.last(),
        Some(Row::ToolResult {
            elapsed: Some(_),
            ..
        })
    ));
}

#[test]
fn a_sub_agent_s_bookkeeping_stays_invisible_except_its_warnings_and_its_end() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    sub(&mut s, "scout", AgentEvent::TextDelta("bubble".into()));
    let before = shape(&s);

    for event in [
        AgentEvent::TurnStart(1),
        usage(9, 9, 0, 0, true),
        AgentEvent::TodoUpdate {
            todos: vec![todo("child step", "pending")],
        },
        AgentEvent::Compressed {
            dropped: 2,
            folded_ids: vec![],
            kept: 1,
            trigger: CompressTrigger::Preflight,
            estimated_tokens_before: 10,
            estimated_tokens_after: 5,
            measured_tokens_before: None,
        },
        AgentEvent::Pruned {
            messages: 1,
            chars_freed: 20,
        },
        AgentEvent::MaxTokensSalvage,
        AgentEvent::CodeLog("console.log".into()),
        AgentEvent::ToolInputDelta {
            id: "c9".into(),
            name: "shell".into(),
            delta: "{".into(),
        },
    ] {
        sub(&mut s, "scout", event);
    }
    assert_eq!(shape(&s), before, "the child's internal events add no row");
    assert!(s.plan.is_empty(), "…and no plan");
    assert_eq!(
        totals(&s),
        [0; 4],
        "…and no bill (the agent level already folded it)"
    );
    assert_eq!(s.run.activity, "thinking");
    assert_eq!(
        s.run.sub_streaming_name.as_deref(),
        Some("scout"),
        "the bubble stays open"
    );
    assert!(s.run.tool_building.is_none());

    sub(&mut s, "scout", AgentEvent::Warning("careful".into()));
    assert_eq!(notices(&s), ["[scout] [warn] careful"]);

    sub(
        &mut s,
        "scout",
        AgentEvent::Done {
            stop_reason: "end_turn".into(),
        },
    );
    assert!(
        s.run.sub_streaming_name.is_none(),
        "the child's last word is final"
    );
    assert!(s.run.thinking_stream.is_none());
    assert_eq!(
        notices(&s),
        ["[scout] [warn] careful"],
        "a normal child stop is silent too"
    );
}

// ── Messages ──────────────────────────────────────────────────────────

#[test]
fn enter_sends_the_message_and_starts_the_run() {
    let (mut s, rx, _cancel) = session();
    typed(&mut s, "hello");
    assert_eq!(enter(&mut s), Effect::None);

    assert_eq!(drain(&rx), vec![text_msg("hello")]);
    assert_eq!(shape(&s), [HELLO, "U:hello"]);
    assert_eq!(s.run.phase, RunPhase::Running);
    assert!(s.run.run_start.is_some());
    assert!(s.run.thinking);
    assert_eq!(s.run.activity, "thinking");
    assert!(s.composer.input.is_empty());
    assert_eq!(history(&s), ["hello"]);
}

#[test]
fn resending_the_same_message_does_not_double_the_history() {
    let (mut s, _rx, _cancel) = session();
    for _ in 0..2 {
        assert_eq!(command(&mut s, "hello"), Effect::None);
        msg(&mut s, UiMsg::RunFinished);
    }
    assert_eq!(history(&s), ["hello"]);
    assert_eq!(command(&mut s, "something else"), Effect::None);
    msg(&mut s, UiMsg::RunFinished);
    assert_eq!(history(&s), ["hello", "something else"]);
    assert_eq!(shape(&s), [HELLO, "U:hello", "U:hello", "U:something else"]);
}

#[test]
fn a_trailing_backslash_continues_the_line_instead_of_sending() {
    let (mut s, rx, _cancel) = session();
    typed(&mut s, "line one \\");
    assert_eq!(enter(&mut s), Effect::None);
    assert_eq!(s.composer.input.text(), "line one \n");
    assert_eq!(s.run.phase, RunPhase::Idle);
    assert!(drain(&rx).is_empty());
}

#[test]
fn enter_on_an_empty_or_blank_composer_does_nothing() {
    let (mut s, rx, _cancel) = session();
    assert_eq!(enter(&mut s), Effect::None);
    typed(&mut s, "   ");
    assert_eq!(enter(&mut s), Effect::None);
    assert_eq!(s.composer.input.text(), "   ");
    assert!(drain(&rx).is_empty());
    assert!(history(&s).is_empty());
}

#[test]
fn a_message_typed_during_a_run_waits_in_the_queue() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    typed(&mut s, "later");
    assert_eq!(enter(&mut s), Effect::None);
    assert_eq!(queued(&s), ["later"]);
    assert!(drain(&rx).is_empty(), "the worker is busy: nothing is sent");
    assert_eq!(
        shape(&s),
        [HELLO, "U:go"],
        "pending text is not part of the conversation yet"
    );
    assert_eq!(s.run.phase, RunPhase::Running);

    // It goes out, in order, the moment the run ends.
    msg(&mut s, UiMsg::RunFinished);
    assert_eq!(drain(&rx), vec![text_msg("later")]);
    assert_eq!(shape(&s), [HELLO, "U:go", "U:later"]);
    assert!(queued(&s).is_empty());
}

#[test]
fn a_message_that_cannot_reach_the_worker_is_not_shown_as_sent() {
    let (mut s, rx, _cancel) = session();
    drop(rx);
    assert_eq!(command(&mut s, "hello"), Effect::None);
    assert_eq!(s.run.phase, RunPhase::Failed);
    assert!(s.run.run_start.is_none());
    assert_eq!(
        shape(&s),
        [
            HELLO,
            "![worker stopped — message not delivered; type /quit to exit]"
        ],
        "the agent never saw it: the transcript must not claim it did"
    );
}

// ── Slash commands ────────────────────────────────────────────────────

#[test]
fn idle_only_commands_are_refused_while_a_run_is_in_flight() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    for (cmd, refused) in [
        ("/clear", "[busy — /clear is disabled while running]"),
        ("/context", "[busy — /context is disabled while running]"),
        ("/compact", "[busy — /compact is disabled while running]"),
        (
            "/compact the bug hunt",
            "[busy — /compact is disabled while running]",
        ),
    ] {
        assert_eq!(command(&mut s, cmd), Effect::None, "{cmd}");
        assert_eq!(
            s.composer.input.text(),
            cmd,
            "{cmd}: kept for an idle retry"
        );
        assert_eq!(latest_notice(&s), refused);
    }
    assert_eq!(levels(&s), [SysLevel::Warn; 4]);
    assert_eq!(s.run.phase, RunPhase::Running);
    assert!(
        drain(&rx).is_empty(),
        "every refusal stayed in the composer"
    );
}

#[test]
fn a_command_that_does_not_parse_stays_for_correction() {
    let (mut s, rx, _cancel) = session();
    for name in ["Model", "clear now", "out 0", "out x"] {
        assert_eq!(command(&mut s, &format!("/{name}")), Effect::None);
        assert_eq!(s.composer.input.text(), format!("/{name}"));
        assert_eq!(
            latest_notice(&s),
            format!("[unknown command: /{name} — try /help]")
        );
    }
    assert_eq!(s.run.phase, RunPhase::Idle);
    assert!(drain(&rx).is_empty());
    assert!(history(&s).is_empty());
}

#[test]
fn info_commands_answer_in_the_transcript_and_leave_the_worker_alone() {
    let (mut s, rx, _cancel) = session();
    for (cmd, answer_starts) in [
        ("/model", "model test-model · provider Openai"),
        ("/limits", "limits: max_turns "),
        ("/tools", "tools: "),
        ("/system", "system (configured base): "),
        ("/help", "commands — session:"),
    ] {
        assert_eq!(command(&mut s, cmd), Effect::None, "{cmd}");
        let answer = latest_notice(&s);
        assert!(answer.starts_with(answer_starts), "{cmd}: {answer}");
        assert!(
            s.composer.input.is_empty(),
            "{cmd}: the command clears the composer"
        );
    }
    assert!(
        drain(&rx).is_empty(),
        "answered locally: the worker is not asked"
    );
    assert!(
        history(&s).is_empty(),
        "commands are not recalled as messages"
    );
    assert_eq!(s.run.phase, RunPhase::Idle);
}

#[test]
fn clear_resets_the_live_session_and_tells_the_worker() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    ev(&mut s, AgentEvent::TextDelta("an answer".into()));
    ev(&mut s, usage(500, 60, 0, 0, false));
    ev(
        &mut s,
        AgentEvent::TodoUpdate {
            todos: vec![todo("step", "completed")],
        },
    );
    msg(&mut s, UiMsg::RunFinished);
    assert_eq!(s.plan.len(), 1);

    assert_eq!(command(&mut s, "/clear"), Effect::None);
    assert_eq!(drain(&rx), vec![Some(Input::Clear)]);
    assert_eq!(shape(&s), ["![history cleared]"]);
    assert_eq!(s.committed, 0);
    assert_eq!(totals(&s), [0; 4]);
    assert_eq!(s.usage.ctx_tokens, 0);
    assert!(
        s.plan.is_empty(),
        "a stale plan segment would linger otherwise"
    );
}

#[test]
fn quit_and_exit_leave_the_loop_after_signalling_the_worker() {
    for cmd in ["/quit", "/exit"] {
        let (mut s, rx, _cancel) = session();
        assert_eq!(command(&mut s, cmd), Effect::Quit, "{cmd}");
        assert_eq!(drain(&rx), vec![None]);
    }
    // …mid-run too: the quit path cancels the turn and tears down.
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    assert_eq!(command(&mut s, "/quit"), Effect::Quit);
    assert_eq!(drain(&rx), vec![None]);
}

#[test]
fn context_and_compact_ask_the_worker_that_owns_the_agent() {
    let (mut s, rx, _cancel) = session();

    assert_eq!(command(&mut s, "/context"), Effect::None);
    assert_eq!(drain(&rx), vec![Some(Input::Context)]);
    assert_eq!(s.run.phase, RunPhase::Idle);
    assert_eq!(
        shape(&s),
        [HELLO],
        "the report arrives later, as its own message"
    );
    msg(&mut s, UiMsg::Context(breakdown(321)));
    assert!(latest_notice(&s).contains("window: "));

    assert_eq!(command(&mut s, "/compact "), Effect::None);
    assert_eq!(drain(&rx), vec![Some(Input::Compact(None))]);
    assert_eq!(s.run.phase, RunPhase::Running);
    msg(&mut s, compact_done(false, false, None));

    assert_eq!(command(&mut s, "/compact the bug hunt"), Effect::None);
    assert_eq!(
        drain(&rx),
        vec![Some(Input::Compact(Some("the bug hunt".into())))]
    );
}

#[test]
fn a_dead_worker_is_reported_by_context_compact_and_clear() {
    let (mut s, rx, _cancel) = session();
    drop(rx);

    assert_eq!(command(&mut s, "/context"), Effect::None);
    assert_eq!(s.run.phase, RunPhase::Idle);
    assert_eq!(
        latest_notice(&s),
        "[worker stopped — context report unavailable]"
    );

    assert_eq!(command(&mut s, "/compact "), Effect::None);
    assert_eq!(
        s.run.phase,
        RunPhase::Idle,
        "not busy: nothing will ever finish it"
    );
    assert_eq!(latest_notice(&s), "[worker stopped — compact unavailable]");

    assert_eq!(command(&mut s, "/clear"), Effect::None);
    assert_eq!(
        shape(&s),
        ["![worker stopped — UI cleared but session history not cleared]"],
        "the agent's history is still there: say so, do not claim a clear"
    );
}

#[test]
fn export_writes_a_new_file_and_never_clobbers_an_existing_one() {
    let (mut s, rx, _cancel) = session();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let path = std::env::temp_dir().join(format!(
        "tinyagent-tui-export-test-{}-{nanos}.md",
        std::process::id()
    ));
    let shown = path.to_string_lossy().into_owned();
    assert!(
        !path.exists(),
        "a stale file from an earlier run would void the test"
    );

    assert_eq!(command(&mut s, &format!("/export {shown}")), Effect::None);
    assert_eq!(latest_notice(&s), format!("[exported 1 rows to {shown}]"));
    assert!(s.composer.input.is_empty());
    let text = std::fs::read_to_string(&path).expect("the export landed");
    std::fs::remove_file(&path).expect("clean up");
    assert!(text.contains("session ready"), "{text}");

    // The TUI has no undo: an existing file is refused untouched, and the
    // command comes back for a different path.
    std::fs::write(&path, "precious").expect("seed the file to be protected");
    assert_eq!(command(&mut s, &format!("/export {shown}")), Effect::None);
    let kept = std::fs::read_to_string(&path).expect("still there");
    std::fs::remove_file(&path).expect("clean up");
    assert_eq!(kept, "precious", "the existing file was not touched");
    assert_eq!(levels(&s).last(), Some(&SysLevel::Error));
    let refusal = latest_notice(&s);
    assert!(refusal.contains("file already exists"), "{refusal}");
    assert_eq!(
        s.composer.input.text(),
        format!("/export {shown}"),
        "back for correction"
    );
    assert!(drain(&rx).is_empty());
}

#[test]
fn queue_lists_what_is_held_and_unqueue_withdraws_the_newest() {
    let (mut s, rx, _cancel) = session();
    failed_with_queue(&mut s, &rx, &["first", "second"]);

    assert_eq!(command(&mut s, "/queue"), Effect::None);
    assert_eq!(
        notices(&s),
        [
            "[error] boom",
            "[queue paused — 2 message(s) held; /resume continues, /unqueue edits]",
            "[queued #1 · not sent]\nfirst",
            "[queued #2 · not sent]\nsecond",
        ]
    );

    assert_eq!(command(&mut s, "/unqueue"), Effect::None);
    assert_eq!(s.composer.input.text(), "second");
    assert_eq!(queued(&s), ["first"]);
    assert!(s.queue_paused, "one message is still held");

    assert_eq!(command(&mut s, "/unqueue"), Effect::None);
    assert_eq!(s.composer.input.text(), "first");
    assert!(queued(&s).is_empty());
    assert!(
        !s.queue_paused,
        "with the last held message gone the pause lifts"
    );

    assert_eq!(command(&mut s, "/queue"), Effect::None);
    assert_eq!(latest_notice(&s), "[queue empty]");
    assert_eq!(command(&mut s, "/unqueue"), Effect::None);
    assert_eq!(latest_notice(&s), "[queue empty — nothing to withdraw]");
    assert_eq!(
        s.composer.input.text(),
        "",
        "nothing came back into the composer"
    );
    assert!(
        drain(&rx).is_empty(),
        "nothing was sent behind the user's back"
    );
}

#[test]
fn resume_releases_a_paused_queue() {
    let (mut s, rx, _cancel) = session();
    failed_with_queue(&mut s, &rx, &["first", "second"]);

    assert_eq!(command(&mut s, "/resume"), Effect::None);
    assert_eq!(drain(&rx), vec![text_msg("first")]);
    assert_eq!(s.run.phase, RunPhase::Running);
    assert_eq!(queued(&s), ["second"]);
    assert!(!s.queue_paused);

    // While a run is in flight the command explains itself instead.
    assert_eq!(command(&mut s, "/resume"), Effect::None);
    assert_eq!(
        latest_notice(&s),
        "[busy — /resume continues a paused queue after the run]"
    );
    assert_eq!(s.run.phase, RunPhase::Running);
    assert!(drain(&rx).is_empty(), "the refusal sends nothing");

    // Released once, the rest of the queue flows like any other.
    msg(&mut s, UiMsg::RunFinished);
    assert_eq!(drain(&rx), vec![text_msg("second")]);
    assert!(queued(&s).is_empty());
}

#[test]
fn resume_without_a_paused_queue_says_so() {
    let (mut s, rx, _cancel) = session();
    assert_eq!(command(&mut s, "/resume"), Effect::None);
    assert_eq!(notices(&s), ["[no paused queue]"]);
    assert_eq!(levels(&s), [SysLevel::Info]);
    assert_eq!(s.run.phase, RunPhase::Idle);
    assert!(drain(&rx).is_empty());
}

#[test]
fn recall_commands_reprint_a_past_detail_or_say_what_exists() {
    let (mut s, rx, _cancel) = session();
    for (cmd, missing) in [
        ("/out ", "[no tool result #1 — 0 result(s) so far]"),
        ("/out 2", "[no tool result #2 — 0 result(s) so far]"),
        ("/call ", "[no tool call #1 — 0 call(s) so far]"),
        ("/thinking ", "[no thinking block #1 — 0 block(s) so far]"),
    ] {
        assert_eq!(command(&mut s, cmd), Effect::None);
        assert_eq!(latest_notice(&s), missing, "{cmd}");
        assert_eq!(
            s.composer.input.text(),
            cmd.trim_end(),
            "{cmd}: kept, the index can be fixed"
        );
    }

    in_flight(&mut s, &rx);
    ev(&mut s, AgentEvent::ThinkingDelta("the reasoning".into()));
    ev(&mut s, tool_call("read_file", json!({"path": "a.rs"})));
    ev(&mut s, tool_result("read_file", "fn main() {}"));
    msg(&mut s, UiMsg::RunFinished);

    // Reprints are appended as OPEN copies; the originals are left as they are.
    let n = s.rows.len();
    let originals = s.rows.clone();
    assert_eq!(command(&mut s, "/out "), Effect::None);
    assert!(matches!(
        s.rows.last(),
        Some(Row::ToolResult { expanded: true, .. })
    ));
    assert_eq!(command(&mut s, "/call "), Effect::None);
    assert!(matches!(
        s.rows.last(),
        Some(Row::ToolCall { expanded: true, .. })
    ));
    assert_eq!(command(&mut s, "/thinking "), Effect::None);
    assert_eq!(shape(&s).last().unwrap(), "T:the reasoning");
    assert_eq!(s.rows.len(), n + 3);
    assert!(
        same_rows(&s.rows[..n], &originals),
        "the committed originals were not touched"
    );

    // Two tool-call rows exist now (the original and its reprint).
    assert_eq!(command(&mut s, "/call 3"), Effect::None);
    assert_eq!(latest_notice(&s), "[no tool call #3 — 2 call(s) so far]");
}

#[test]
fn ctrl_o_reprints_the_latest_detail() {
    let (mut s, rx, _cancel) = session();
    assert_eq!(ctrl(&mut s, 'o'), Effect::None);
    assert_eq!(
        latest_notice(&s),
        "[no tool call, result or thinking detail yet]"
    );

    in_flight(&mut s, &rx);
    ev(&mut s, tool_call("read_file", json!({"path": "a.rs"})));
    ev(&mut s, tool_result("read_file", "fn main() {}"));
    let before = s.rows.len();
    assert_eq!(ctrl(&mut s, 'o'), Effect::None);
    assert_eq!(s.rows.len(), before + 1);
    assert!(
        matches!(s.rows.last(), Some(Row::ToolResult { expanded: true, .. })),
        "the newest detail-bearing row wins"
    );
}

// ── Keys ──────────────────────────────────────────────────────────────

#[test]
fn arrow_keys_recall_messages_not_commands() {
    let (mut s, rx, _cancel) = session();
    for text in ["one", "two"] {
        assert_eq!(command(&mut s, text), Effect::None);
        msg(&mut s, UiMsg::RunFinished);
    }
    assert_eq!(command(&mut s, "/model"), Effect::None);

    let up = |s: &mut Session| key(s, KeyCode::Up, KeyModifiers::NONE);
    let down = |s: &mut Session| key(s, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(up(&mut s), Effect::None);
    assert_eq!(s.composer.input.text(), "two");
    up(&mut s);
    assert_eq!(s.composer.input.text(), "one");
    down(&mut s);
    assert_eq!(s.composer.input.text(), "two");
    down(&mut s);
    assert_eq!(
        s.composer.input.text(),
        "",
        "past the newest: the draft comes back"
    );
    up(&mut s);
    assert_eq!(s.composer.input.text(), "two");
    assert_eq!(drain(&rx).len(), 2, "recall never sends");
}

#[test]
fn ctrl_c_cancels_a_run_and_quits_only_from_idle_on_a_double_press() {
    let (mut s, rx, cancel) = session();
    in_flight(&mut s, &rx);

    // While a run is in flight: cancel, stay.
    assert_eq!(ctrl(&mut s, 'c'), Effect::None);
    assert_eq!(s.run.phase, RunPhase::Cancelling);
    assert!(cancel.load(Ordering::Relaxed));
    msg(&mut s, UiMsg::RunFinished);

    // Idle: the first press only asks for confirmation…
    assert_eq!(ctrl(&mut s, 'c'), Effect::None);
    assert_eq!(latest_notice(&s), "[press Ctrl-C again to quit]");
    assert!(drain(&rx).is_empty(), "nothing was told to stop");

    // …the second press quits.
    assert_eq!(ctrl(&mut s, 'c'), Effect::Quit);
    assert_eq!(drain(&rx), vec![None]);
}

#[test]
fn idle_esc_clears_the_input_and_never_quits() {
    let (mut s, rx, _cancel) = session();
    typed(&mut s, "a draft");
    assert_eq!(esc(&mut s), Effect::None);
    assert!(s.composer.input.is_empty());
    assert!(drain(&rx).is_empty());
}

#[test]
fn ctrl_d_deletes_forward_and_quits_only_on_an_empty_idle_composer() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    assert_eq!(
        ctrl(&mut s, 'd'),
        Effect::None,
        "empty + busy: nothing to delete, no quit"
    );
    assert_eq!(s.run.phase, RunPhase::Running);
    msg(&mut s, UiMsg::RunFinished);

    typed(&mut s, "ab");
    key(&mut s, KeyCode::Home, KeyModifiers::NONE);
    assert_eq!(ctrl(&mut s, 'd'), Effect::None);
    assert_eq!(s.composer.input.text(), "b");

    ctrl(&mut s, 'k');
    assert!(s.composer.input.is_empty());
    assert_eq!(ctrl(&mut s, 'd'), Effect::Quit);
    assert_eq!(drain(&rx), vec![None]);
}

#[test]
fn alt_enter_inserts_a_newline_and_released_keys_are_ignored() {
    let (mut s, rx, _cancel) = session();
    typed(&mut s, "first");
    assert_eq!(key(&mut s, KeyCode::Enter, KeyModifiers::ALT), Effect::None);
    assert_eq!(s.composer.input.text(), "first\n");
    assert!(drain(&rx).is_empty());

    // A key release must not act a second time on its press…
    let release = KeyEvent {
        code: KeyCode::Enter,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Release,
        state: crossterm::event::KeyEventState::NONE,
    };
    assert_eq!(s.on_key(release, &cfg()), Effect::None);
    assert!(drain(&rx).is_empty());

    // …while a repeat counts as a press (held-key terminals).
    let repeat = KeyEvent {
        kind: KeyEventKind::Repeat,
        ..release
    };
    assert_eq!(s.on_key(repeat, &cfg()), Effect::None);
    assert_eq!(drain(&rx), vec![text_msg("first")]);
}

#[test]
fn a_paste_inserts_the_whole_blob_without_sending_any_of_it() {
    let (mut s, rx, _cancel) = session();
    s.on_paste("line one\nline two\u{1b}[31m");
    assert_eq!(s.composer.input.text(), "line one\nline two");
    assert!(drain(&rx).is_empty(), "a paste is not a run of Enters");

    // Escape sequences go whole: a colour or a hyperlink leaves nothing
    // behind, CRLF folds to a newline, the tab survives, DEL goes.
    s.on_paste(
        "\u{1b}[1;32mgreen\u{1b}[0m \u{1b}]8;;https://example.com\x07link\u{1b}]8;;\x07\ta\r\nb\u{7f}",
    );
    assert_eq!(
        s.composer.input.text(),
        "line one\nline twogreen link\ta\nb"
    );

    // A bare ESC costs only itself.
    s.on_paste("a\u{1b}\nb");
    assert_eq!(
        s.composer.input.text(),
        "line one\nline twogreen link\ta\nba\nb"
    );
}

// ── The slash popup ───────────────────────────────────────────────────

fn popup_names(s: &Session) -> Vec<&str> {
    s.composer.popup.matches.iter().map(|m| m.name).collect()
}

#[test]
fn a_fully_typed_command_runs_on_the_first_enter() {
    let (mut s, rx, _cancel) = session();
    typed(&mut s, "/clear");
    assert!(
        s.composer.popup.is_open(),
        "the match is listed while the name is typed"
    );
    assert_eq!(popup_names(&s), ["clear"]);

    assert_eq!(enter(&mut s), Effect::None);
    assert_eq!(drain(&rx), vec![Some(Input::Clear)]);
    assert_eq!(shape(&s), ["![history cleared]"]);
    assert!(!s.composer.popup.is_open());

    typed(&mut s, "/quit");
    assert_eq!(enter(&mut s), Effect::Quit);
}

#[test]
fn an_abbreviation_completes_on_enter_and_never_runs() {
    let (mut s, rx, _cancel) = session();
    typed(&mut s, "/cl");
    assert_eq!(popup_names(&s), ["clear"]);

    assert_eq!(enter(&mut s), Effect::None);
    assert_eq!(s.composer.input.text(), "/clear", "completed, on screen");
    assert!(
        drain(&rx).is_empty(),
        "NOT run: `/cl` + Enter must not erase the history sight unseen"
    );
    assert!(
        !s.composer.popup.is_open(),
        "and the popup does not pop back on the same text"
    );

    // The second Enter is the informed one.
    assert_eq!(enter(&mut s), Effect::None);
    assert_eq!(drain(&rx), vec![Some(Input::Clear)]);
}

#[test]
fn a_bare_slash_never_runs_its_first_entry() {
    let (mut s, rx, _cancel) = session();
    typed(&mut s, "/");
    assert_eq!(
        popup_names(&s)[0],
        "clear",
        "the table's first entry is the destructive one"
    );
    assert_eq!(enter(&mut s), Effect::None);
    assert_eq!(s.composer.input.text(), "/clear");
    assert!(drain(&rx).is_empty());
}

#[test]
fn tab_completes_without_sending() {
    let (mut s, rx, _cancel) = session();
    let tab = |s: &mut Session| key(s, KeyCode::Tab, KeyModifiers::NONE);

    typed(&mut s, "/ex");
    assert_eq!(tab(&mut s), Effect::None);
    assert_eq!(
        s.composer.input.text(),
        "/export ",
        "room for the argument is opened"
    );
    assert!(!s.composer.popup.is_open());

    s.composer.input.set_text("");
    refresh(&mut s);
    typed(&mut s, "/mo");
    assert_eq!(tab(&mut s), Effect::None);
    assert_eq!(s.composer.input.text(), "/model");
    assert!(drain(&rx).is_empty());
}

#[test]
fn an_argument_taking_command_only_completes_on_enter() {
    let (mut s, rx, _cancel) = session();
    for name in ["export", "compact", "out", "call", "thinking"] {
        s.composer.input.set_text("");
        refresh(&mut s);
        typed(&mut s, &format!("/{name}"));
        assert_eq!(enter(&mut s), Effect::None, "/{name}");
        assert_eq!(
            s.composer.input.text(),
            format!("/{name} "),
            "/{name}: completed, awaiting its argument"
        );
    }
    assert!(
        drain(&rx).is_empty(),
        "none ran: no file, no billed summary, no reprint"
    );
}

#[test]
fn esc_while_the_popup_is_open_only_closes_it() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    typed(&mut s, "/ca");
    assert!(s.composer.popup.is_open());

    assert_eq!(esc(&mut s), Effect::None);
    assert!(!s.composer.popup.is_open());
    assert_eq!(s.composer.input.text(), "/ca", "the text stays for editing");
    assert_eq!(s.run.phase, RunPhase::Running, "the run was NOT cancelled");
    assert!(notices(&s).is_empty());
    // Once the popup is closed the same key means what it always means.
    assert_eq!(esc(&mut s), Effect::None);
    assert_eq!(s.run.phase, RunPhase::Cancelling);
}

#[test]
fn arrow_keys_move_the_popup_selection_instead_of_the_caret_or_history() {
    let (mut s, rx, _cancel) = session();
    assert_eq!(command(&mut s, "one"), Effect::None);
    msg(&mut s, UiMsg::RunFinished);

    typed(&mut s, "/");
    assert_eq!(key(&mut s, KeyCode::Down, KeyModifiers::NONE), Effect::None);
    assert_eq!(key(&mut s, KeyCode::Tab, KeyModifiers::NONE), Effect::None);
    assert_eq!(
        s.composer.input.text(),
        "/export ",
        "the second entry, not the first"
    );

    s.composer.input.set_text("/");
    refresh(&mut s);
    assert_eq!(key(&mut s, KeyCode::Up, KeyModifiers::NONE), Effect::None);
    key(&mut s, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(
        s.composer.input.text(),
        "/quit",
        "Up wraps to the last entry"
    );
    assert_eq!(s.composer.hist_idx, None, "no history recall happened");
    assert_eq!(drain(&rx).len(), 1, "nothing was run or sent");
}

#[test]
fn idle_only_commands_are_hidden_from_the_popup_while_a_run_is_in_flight() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    typed(&mut s, "/");
    let names = popup_names(&s);
    for shown in ["queue", "unqueue", "model", "export", "out"] {
        assert!(names.contains(&shown), "{shown}: {names:?}");
    }
    for hidden in ["clear", "resume", "context", "compact"] {
        assert!(!names.contains(&hidden), "{hidden}: {names:?}");
    }
    typed(&mut s, "c");
    assert_eq!(popup_names(&s), ["call"]);
}

// ── The viewport ──────────────────────────────────────────────────────

/// Paint the live viewport into a fresh cell grid and return its rows
/// (trailing blanks stripped). A fresh terminal each time: the loop's
/// scrollback commits are the terminal's business (`commit` below pins the
/// watermark); this reads what `draw` makes of the session.
fn screen(s: &mut Session) -> Vec<String> {
    rows_of(paint(s))
}

/// The frame the loop leaves painted when it ends (`view::final_frame`).
fn final_screen(s: &mut Session) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    view::final_frame(s, &mut term, &cfg());
    rows_of(term)
}

fn rows_of(term: Terminal<TestBackend>) -> Vec<String> {
    let buf = term.backend().buffer();
    (0..H)
        .map(|y| {
            (0..W)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn paint(s: &mut Session) -> Terminal<TestBackend> {
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    term.draw(|f| view::draw(f, s, &cfg())).unwrap();
    term
}

#[test]
fn the_first_screen_shows_the_masthead_until_the_user_types() {
    let (mut s, rx, _cancel) = session();
    let rows = screen(&mut s);
    assert_eq!(rows.len(), 8);
    assert!(rows[0].contains("T I N Y A G E N T"), "{rows:?}");
    assert!(
        rows[2].contains("A small agent for real terminal work."),
        "{rows:?}"
    );
    assert!(rows[4].contains("◆ MESSAGE"), "{rows:?}");
    assert!(rows[5].contains("Describe a task…"), "{rows:?}");
    assert!(
        rows[6].contains("Enter") && rows[6].contains("send"),
        "{rows:?}"
    );
    assert!(rows[7].contains("ready"), "{rows:?}");

    typed(&mut s, "he");
    let rows = screen(&mut s);
    assert!(
        !rows.iter().any(|r| r.contains("T I N Y")),
        "the masthead makes room for the conversation: {rows:?}"
    );
    assert!(rows[5].contains("❯ he"), "{rows:?}");
    assert!(drain(&rx).is_empty());
}

#[test]
fn quitting_leaves_no_completion_popup_painted_on_screen() {
    // The inline viewport is released, not erased: the last frame painted
    // stays above the shell prompt. A `/quit` typed in full runs on the
    // first Enter — straight out of the open popup — so the loop repaints
    // once with the popup closed on its way out.
    let (mut s, rx, _cancel) = session();
    typed(&mut s, "/quit");
    let rows = screen(&mut s);
    assert!(
        rows.iter().any(|r| r.contains("exit the TUI")),
        "the popup is up while the name is typed: {rows:?}"
    );
    assert_eq!(enter(&mut s), Effect::Quit);
    assert_eq!(drain(&rx), vec![None]);
    let rows = final_screen(&mut s);
    assert!(
        !rows
            .iter()
            .any(|r| r.contains("exit the TUI") || r.contains("select")),
        "the popup stays painted: {rows:?}"
    );
    // The command took the text like every executed command does: what is
    // left on screen is the plain idle composer.
    assert!(
        rows.iter().any(|r| r.contains("Describe a task")),
        "the composer is back to idle: {rows:?}"
    );

    // The double Ctrl-C quits from wherever the user was: a half-typed
    // command's popup goes the same way.
    let (mut s, _rx, _cancel) = session();
    typed(&mut s, "/cl");
    assert!(screen(&mut s).iter().any(|r| r.contains("/clear")));
    assert_eq!(ctrl(&mut s, 'c'), Effect::None);
    assert_eq!(ctrl(&mut s, 'c'), Effect::Quit);
    let rows = final_screen(&mut s);
    assert!(!rows.iter().any(|r| r.contains("/clear")), "{rows:?}");
}

#[test]
fn the_window_note_rides_the_welcome_row_and_leaves_the_masthead_visible() {
    // A default config leaves the model window unset. The note used to
    // arrive as its own warning message — four wrapped yellow lines that
    // pushed the transcript past the empty screen and buried the masthead.
    let note = crate::config::Limits::default()
        .context_window_hint_line()
        .expect("the default config leaves the window unset");
    let (tx, _rx) = mpsc::channel();
    let mut s = Session::new(
        Startup {
            model: "test-model",
            n_tools: 0,
            initial_task: None,
            auto_start: false,
            resume_rows: Vec::new(),
            window_note: Some(&note),
        },
        tx,
        Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(
        s.rows.len(),
        1,
        "the note is not a row of its own: {:?}",
        s.rows
    );
    assert!(
        matches!(
            &s.rows[0],
            Row::System { text, level: SysLevel::Info }
                if text.starts_with("session ready · test-model · 0 tools")
                    && text.contains(&note)
                    && text.contains("/context")
        ),
        "{:?}",
        s.rows[0]
    );

    let rows = screen(&mut s);
    assert!(rows[0].contains("T I N Y A G E N T"), "{rows:?}");
    assert!(
        rows[2].contains("A small agent for real terminal work."),
        "{rows:?}"
    );
    assert_eq!(rows[3].trim(), "", "the breathing row stays: {rows:?}");
}

#[test]
fn a_running_session_shows_what_the_agent_is_doing_above_the_input_box() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);

    let rows = screen(&mut s);
    assert!(
        rows[3].contains("◆ …"),
        "the thinking placeholder: {rows:?}"
    );
    assert!(rows[4].contains("thinking…"), "{rows:?}");
    assert!(rows[4].contains("Ctrl-C cancel"), "{rows:?}");
    assert!(rows[7].contains("running"), "{rows:?}");
    assert!(
        !rows[6].contains("send"),
        "the send hint hides while busy: {rows:?}"
    );

    ev(&mut s, AgentEvent::TextDelta("Hello, live viewport".into()));
    let rows = screen(&mut s);
    assert!(rows[3].contains("Hello, live viewport"), "{rows:?}");
    assert!(rows[4].contains("writing…"), "{rows:?}");

    // A tool streams nothing of its own: the placeholder keeps its timing visible.
    ev(&mut s, tool_call("read_file", json!({"path": "a.rs"})));
    let rows = screen(&mut s);
    assert!(rows[3].contains("→ read_file"), "{rows:?}");
    assert!(rows[4].contains("→ read_file"), "{rows:?}");
}

#[test]
fn a_failed_run_names_the_next_step_in_the_input_title() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    msg(&mut s, UiMsg::Error("boom".into()));
    let rows = screen(&mut s);
    assert!(
        rows[4].contains("failed · type a message to retry"),
        "{rows:?}"
    );
    assert!(rows[7].contains("failed"), "{rows:?}");

    failed_with_queue(&mut s, &rx, &["after"]);
    let rows = screen(&mut s);
    assert!(
        rows[4].contains("failed · /resume continues the held queue"),
        "{rows:?}"
    );
}

#[test]
fn queued_messages_are_previewed_above_the_input_box() {
    let (mut s, rx, _cancel) = session();
    in_flight(&mut s, &rx);
    assert_eq!(command(&mut s, "after the run"), Effect::None);

    let rows = screen(&mut s);
    assert!(
        rows[3].contains("1 queued") && rows[3].contains("after the run"),
        "{rows:?}"
    );
    assert!(
        rows[4].contains("1 queued"),
        "the title counts them too: {rows:?}"
    );

    // A paused queue says how it is released…
    msg(&mut s, UiMsg::Error("boom".into()));
    let rows = screen(&mut s);
    assert!(
        rows[3].contains("1 paused") && rows[3].contains("/resume"),
        "{rows:?}"
    );

    // …and withdrawing the message removes the row.
    assert_eq!(command(&mut s, "/unqueue"), Effect::None);
    let rows = screen(&mut s);
    assert!(!rows[3].contains("paused"), "{rows:?}");
}

#[test]
fn the_popup_takes_over_the_zone_with_a_key_hint() {
    let (mut s, rx, _cancel) = session();
    typed(&mut s, "/");
    let rows = screen(&mut s);
    assert!(rows[0].contains("/clear"), "{rows:?}");
    assert!(
        rows.iter()
            .any(|r| r.contains("Tab") && r.contains("complete")),
        "{rows:?}"
    );
    assert!(rows[5].contains("❯ /"), "{rows:?}");
    assert!(drain(&rx).is_empty());
}

#[test]
fn the_status_bar_reports_cost_context_and_plan_once_there_are_any() {
    let (mut s, _rx, _cancel) = session();
    let idle = screen(&mut s)[7].clone();
    assert!(
        idle.contains("test-model") && idle.contains("ready"),
        "{idle}"
    );
    assert!(
        !idle.contains("plan"),
        "a brand-new session has nothing to report: {idle}"
    );

    ev(&mut s, usage(1000, 40, 0, 0, false));
    ev(
        &mut s,
        AgentEvent::TodoUpdate {
            todos: vec![todo("read", "completed"), todo("write", "pending")],
        },
    );
    let after = screen(&mut s)[7].clone();
    assert!(after.contains("ctx") && after.contains("out"), "{after}");
    assert!(after.contains("plan 1/2"), "{after}");
}

#[test]
fn the_cursor_sits_on_the_caret_in_the_composer() {
    let (mut s, _rx, _cancel) = session();
    assert_eq!(
        paint(&mut s).get_cursor_position().unwrap(),
        Position::new(3, 5)
    );

    typed(&mut s, "你好");
    // Two double-width glyphs: the caret moves four cells, not two.
    assert_eq!(
        paint(&mut s).get_cursor_position().unwrap(),
        Position::new(7, 5)
    );
}

#[test]
fn commit_advances_to_the_open_stream_and_past_every_final_row() {
    let (mut s, _rx, _cancel) = session();
    let mut term = Terminal::new(TestBackend::new(W, 30)).unwrap();
    view::commit(&mut s, &mut term).unwrap();
    assert_eq!(s.committed, 1, "the welcome row is final from the start");

    assert_eq!(command(&mut s, "go"), Effect::None);
    ev(&mut s, AgentEvent::TurnStart(1));
    ev(&mut s, AgentEvent::TextDelta("streaming answer".into()));
    s.dirty = false;
    view::commit(&mut s, &mut term).unwrap();
    assert_eq!(
        s.committed, 2,
        "the user row commits; the open answer waits"
    );
    assert!(s.dirty, "a commit always repaints");

    // Only a TRAILING streaming row is held: rows that land ahead of an open
    // sub-agent bubble are final.
    msg(&mut s, UiMsg::RunFinished);
    assert_eq!(command(&mut s, "next"), Effect::None);
    ev(&mut s, AgentEvent::TurnStart(1));
    sub(&mut s, "scout", AgentEvent::TextDelta("partial".into()));
    ev(&mut s, tool_call("read_file", json!({})));
    assert!(s.run.stream_open(), "the bubble is still open");
    assert_eq!(shape(&s).len(), 6);
    view::commit(&mut s, &mut term).unwrap();
    assert_eq!(s.committed, 6);

    // The bubble resumes in a fresh row, which then waits like any open stream.
    sub(&mut s, "scout", AgentEvent::TextDelta("more".into()));
    sub(&mut s, "scout", AgentEvent::TextDelta(" and more".into()));
    assert_eq!(shape(&s).len(), 7);
    assert_eq!(shape(&s).last().unwrap(), "A:[scout] more and more");
    view::commit(&mut s, &mut term).unwrap();
    assert_eq!(s.committed, 6);

    sub(
        &mut s,
        "scout",
        AgentEvent::Done {
            stop_reason: "end_turn".into(),
        },
    );
    view::commit(&mut s, &mut term).unwrap();
    assert_eq!(s.committed, 7, "closed: the last row commits too");
}
