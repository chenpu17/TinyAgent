//! Tests for the CLI-side headless harness — `run_with_retries` (the retry
//! ladder), result.json writing, NDJSON mode, and the unix real-leaf
//! end-to-end runs. They moved out of the kernel crate's agent_tests.rs when
//! the harness left the library: they exercise CLI behavior, so they compile
//! where the harness lives. The kernel-level loop invariants they relied on
//! stay behind in crates/core.
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tiny_agent::provider::mock::{self, MockProvider, MockTurn};
use tiny_agent::provider::{ChatEvent, ChatRequest, ChatResponse};
use tiny_agent::tools::ToolOutput;
use tiny_agent::{Agent, Block, Config, Message, Provider, Tool, ToolCtx, ToolDef};

// Shared fixtures with the kernel's agent_tests.rs (copied, not imported —
// they live in that crate's cfg(test) scope): a minimal user message, the
// smallest valid config, a scratch ToolCtx, and the recording EchoTool.
fn user_text(s: &str) -> Message {
    Message::user(vec![Block::Text(s.into())])
}

fn cfg_with(max_turns: u32, timeout_secs: u64, compress: usize) -> Config {
    let mut c = serde_json::from_str::<Config>(
            r#"{"provider":"openai","api_key_env":"MOCK_API_KEY","model":"m","inject_environment":false}"#,
        )
        .expect("valid test config");
    c.limits.max_turns = max_turns;
    c.limits.timeout_secs = timeout_secs;
    c.limits.compress_threshold_chars = compress;
    c
}

fn test_ctx() -> ToolCtx {
    ToolCtx {
        tool_absent_notes: Vec::new(),
        workdir: std::env::current_dir().expect("cwd"),
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

/// A no-op tool that can also (a) record that it ran, and (b) flip a
/// shared cancel flag mid-run — to exercise the batch-cancel path.
struct EchoTool {
    name: String,
    ran: Option<Arc<AtomicBool>>,
    cancel_on_run: Option<Arc<AtomicBool>>,
}
impl EchoTool {
    fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            ran: None,
            cancel_on_run: None,
        }
    }
    fn tracking(name: &str, ran: Arc<AtomicBool>) -> Self {
        Self {
            name: name.into(),
            ran: Some(ran),
            cancel_on_run: None,
        }
    }
}
impl Tool for EchoTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: self.name.clone(),
            description: "test echo tool".into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }
    fn run(&self, _input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        if let Some(r) = &self.ran {
            r.store(true, Ordering::Relaxed);
        }
        if let Some(c) = &self.cancel_on_run {
            c.store(true, Ordering::Relaxed);
        }
        Ok(ToolOutput::text("ok"))
    }
}

#[test]
fn single_run_retries_transient_then_succeeds() {
    // harness::run_with_retries retries on agent.run() Err: two transient
    // 503s then a clean turn → success, exit 0, attempts==3 in result.json.
    // The injected sleeper is a no-op so the test doesn't wait on backoff.
    let cfg = cfg_with(5, 0, 0);
    let (prov, state) = MockProvider::new(vec![
        MockTurn::Err("503 transient".into()),
        MockTurn::Err("503 transient".into()),
        mock::resp_text("recovered", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("go")],
        test_ctx(),
    );
    let dir = std::env::temp_dir().join(format!("ta-retry-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let out = dir.join("r.json");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 2, |_| {}).unwrap();
    assert_eq!(code, 0, "recovered → exit 0");
    assert_eq!(state.calls(), 3, "two retries → three attempts");
    let s = std::fs::read_to_string(&out).unwrap();
    assert!(s.contains("\"attempts\": 3"), "attempts not recorded: {s}");
    assert!(s.contains("\"success\": true"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn single_run_started_at_reflects_first_attempt_not_write_time() {
    // C1: result.json's started_at must stamp the run's true beginning —
    // including the retry backoff. Pre-fix it was taken at write time, so
    // a retried run's backoff waits were erased (started_at == finished_at
    // at second precision). Real sleeps here (backoff 1s + 2s ≥ 2.25s
    // even with full negative jitter) make the second-resolution
    // timestamps differ deterministically.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _state) = MockProvider::new(vec![
        MockTurn::Err("503 transient".into()),
        MockTurn::Err("503 transient".into()),
        mock::resp_text("recovered", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("go")],
        test_ctx(),
    );
    let dir = std::env::temp_dir().join(format!("ta-retry-start-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let out = dir.join("r.json");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 2, |d| {
        std::thread::sleep(d)
    })
    .unwrap();
    assert_eq!(code, 0);
    let s = std::fs::read_to_string(&out).unwrap();
    let field = |k: &str| {
        s.split(&format!("\"{k}\": \""))
            .nth(1)
            .unwrap_or_else(|| panic!("no {k} in: {s}"))
            .split('"')
            .next()
            .unwrap()
            .to_string()
    };
    let (started, finished) = (field("started_at"), field("finished_at"));
    assert!(
        started < finished,
        "started_at must precede finished_at across retried runs: {started} vs {finished}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn single_run_write_failure_keeps_exit_code() {
    // C3: result.json 写失败不能吞掉 run 自己的 exit code——旧代码的 `?`
    // 会把任何非写失败退化成 exit 1，对 CI 谎报实际发生了什么（例如
    // cancelled 的 130 被改成 1）。
    //
    // 失败注入：把输出的父路径弄成一个已存在的普通文件。atomic_write 先
    // create_dir_all 父目录，那个名字已被一个非目录占用，于是必然报
    // AlreadyExists(EEXIST) 而返回 Err，一个字节都不会落盘。这条失败由
    // 文件系统结构决定，与运行用户身份无关——此前用 chmod 0o555 只读目录
    // 注入，root 对只读目录照样能写，本机（root）下会被误判成"写成功了"。
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![MockTurn::Err("503 one".into())]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("go")],
        test_ctx(),
    );
    let dir = std::env::temp_dir().join(format!("ta-retry-wfail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let blocker = dir.join("sub");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let out = blocker.join("r.json");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {})
        .expect("write failure must not turn into Err");
    assert_eq!(code, 1, "the run's own exit code is preserved");
    assert!(!out.exists(), "nothing was written");
    // 父路径仍是那个普通文件，没被替换成目录。
    assert!(blocker.is_file(), "blocker 文件不应被改动");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unfixable_overflow_run_error_is_deterministic() {
    // B4: an overflow the escape hatch can't fix (history uncompressible —
    // keep_recent too large for a safe cut) surfaces as a RunError whose
    // is_context_overflow() is true; harness/batch use that to skip the
    // transient retry ladder. A plain transient error is NOT overflow.
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.keep_recent_messages = 100; // n <= keep+2 → no safe cut
    let (prov, _) = MockProvider::new(vec![MockTurn::Err(
        "mock: maximum context length is 8192 tokens, got 9000".into(),
    )]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("go")],
        test_ctx(),
    );
    let re = match agent.run(&mut |_| {}) {
        Err(e) => e,
        Ok(o) => panic!("expected Err, got Ok({})", o.stop_reason),
    };
    assert!(re.is_context_overflow(), "source: {re:#}");

    let (prov2, _) = MockProvider::new(vec![MockTurn::Err("503 transient".into())]);
    let mut agent2 = Agent::new(
        &cfg,
        Box::new(prov2),
        vec![],
        vec![user_text("go")],
        test_ctx(),
    );
    let re2 = match agent2.run(&mut |_| {}) {
        Err(e) => e,
        Ok(o) => panic!("expected Err, got Ok({})", o.stop_reason),
    };
    assert!(!re2.is_context_overflow());
}

#[test]
fn single_run_does_not_retry_unfixable_overflow() {
    // B4 companion: the retry ladder must not fire on deterministic
    // overflow — retries=3 with a single scripted Err still makes exactly
    // ONE provider call (a retry would consume the script and surface
    // "turns exhausted" on later attempts, inflating calls()).
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.keep_recent_messages = 100;
    let (prov, state) = MockProvider::new(vec![MockTurn::Err(
        "mock: maximum context length exceeded".into(),
    )]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("go")],
        test_ctx(),
    );
    let dir = std::env::temp_dir().join(format!("ta-retry-ovf-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let out = dir.join("r.json");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 3, |_| {}).unwrap();
    assert_eq!(code, 1);
    assert_eq!(
        state.calls(),
        1,
        "deterministic overflow must not enter the retry ladder"
    );
    // B4 vocabulary alignment: the single-run result.json must say
    // "context_overflow" (like batch's report), not the generic "error" —
    // CI keys off this value to tell a too-long task from a flaky provider.
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).expect("result.json written")).unwrap();
    assert_eq!(written["stop_reason"], "context_overflow");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn single_run_folds_failed_attempt_cost_into_final_totals() {
    // P2-3 (honest cost): a retried run's result.json must bill EVERY
    // attempt, not just the last. Attempt 1 bills 100/10 streamed tokens
    // and runs a tool (EchoTool) on turn 1, then errors on turn 2; the
    // RunError carries that partial cost. Attempt 2 succeeds billing 50/5.
    // The final outcome must fold the failed attempt's 100/10 + its tool
    // call into the totals — otherwise the real cost (tokens spent, tool
    // run, side effects on disk) is silently dropped on a retry.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _state) = MockProvider::new(vec![
        // Attempt 1, turn 1: tool_use that bills streamed usage, then…
        mock::resp_tool("t", "echo", json!({}), mock::mk_usage(100, 10)),
        // Attempt 1, turn 2: provider error → run fails carrying turn 1's cost.
        MockTurn::Err("503 transient".into()),
        // Attempt 2, turn 1: clean recovery.
        mock::resp_text("done", mock::mk_usage(50, 5)),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(EchoTool::new("echo"))],
        vec![user_text("go")],
        test_ctx(),
    );
    let dir = std::env::temp_dir().join(format!("ta-retry-fold-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let out = dir.join("r.json");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 2, |_| {}).unwrap();
    assert_eq!(code, 0, "recovered → exit 0");
    let s = std::fs::read_to_string(&out).unwrap();
    // Folded: 100 (failed attempt) + 50 (success) input; 10 + 5 output.
    assert!(
        s.contains("\"input_tokens\": 150"),
        "failed attempt's input tokens not folded in: {s}"
    );
    assert!(
        s.contains("\"output_tokens\": 15"),
        "failed attempt's output tokens not folded in: {s}"
    );
    // The failed attempt's tool call survives in the audit list.
    assert!(
        s.contains("\"name\": \"echo\""),
        "failed attempt's tool call not preserved: {s}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn single_run_retries_exhausted_exit_1() {
    // All attempts fail: exit 1, attempts==retries+1, result.json carries
    // the last error (stop_reason "error"). Deterministic Ok outcomes are
    // never retried — only Err is, which is exactly what the mock feeds.
    let cfg = cfg_with(5, 0, 0);
    let (prov, state) = MockProvider::new(vec![
        MockTurn::Err("503 one".into()),
        MockTurn::Err("503 two".into()),
        MockTurn::Err("503 three".into()),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("go")],
        test_ctx(),
    );
    let dir = std::env::temp_dir().join(format!("ta-retry2-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let out = dir.join("r.json");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 2, |_| {}).unwrap();
    assert_eq!(code, 1, "exhausted → exit 1");
    assert_eq!(state.calls(), 3, "retries=2 → 3 total attempts, no more");
    let s = std::fs::read_to_string(&out).unwrap();
    assert!(s.contains("\"attempts\": 3"), "{s}");
    assert!(s.contains("\"success\": false"));
    assert!(s.contains("\"stop_reason\": \"error\""));
    assert!(s.contains("503"), "last error surfaced: {s}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn single_run_all_attempts_fail_still_bills_accumulated_cost() {
    // P2: when EVERY attempt fails (retries exhausted), the failure
    // result.json must still bill the tokens each attempt burned — the
    // None-outcome path folds `acc` in, it doesn't zero everything. Each
    // attempt bills on turn 1 (a tool_use) then errors on turn 2; the
    // RunError carries that turn's usage, acc accumulates across attempts,
    // and the final failure record reports the sum.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _state) = MockProvider::new(vec![
        // Attempt 1: bill 100/10 via a tool call, then error.
        mock::resp_tool("t1", "echo", json!({}), mock::mk_usage(100, 10)),
        MockTurn::Err("503 transient".into()),
        // Attempt 2: bill 50/5 via a tool call, then error.
        mock::resp_tool("t2", "echo", json!({}), mock::mk_usage(50, 5)),
        MockTurn::Err("503 transient".into()),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(EchoTool::new("echo"))],
        vec![user_text("go")],
        test_ctx(),
    );
    let dir = std::env::temp_dir().join(format!("ta-retry-fail-cost-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let out = dir.join("r.json");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 1, |_| {}).unwrap();
    assert_eq!(code, 1, "all attempts failed → exit 1");
    let s = std::fs::read_to_string(&out).unwrap();
    assert!(s.contains("\"success\": false"), "{s}");
    // Accumulated honest cost across both failed attempts: 100+50 input,
    // 10+5 output — NOT zeroed just because no attempt succeeded.
    assert!(
        s.contains("\"input_tokens\": 150"),
        "failed attempts' input tokens dropped on the failure path: {s}"
    );
    assert!(
        s.contains("\"output_tokens\": 15"),
        "failed attempts' output tokens dropped on the failure path: {s}"
    );
    // Both attempts' tool calls survive in the audit list.
    assert!(
        s.matches("\"name\": \"echo\"").count() == 2,
        "expected both attempts' tool calls in the failure record: {s}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn single_run_no_retry_when_retries_zero() {
    // retries=0: a single Err → exit 1, attempts==1, no retry attempted.
    let cfg = cfg_with(5, 0, 0);
    let (prov, state) = MockProvider::new(vec![MockTurn::Err("503".into())]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("go")],
        test_ctx(),
    );
    let dir = std::env::temp_dir().join(format!("ta-retry0-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let out = dir.join("r.json");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
    assert_eq!(code, 1);
    assert_eq!(state.calls(), 1, "retries=0 → exactly one attempt");
    let s = std::fs::read_to_string(&out).unwrap();
    assert!(s.contains("\"attempts\": 1"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn single_run_cancel_during_backoff_exits_130() {
    // A Ctrl-C during the retry backoff must cancel (exit 130) instead of
    // sleeping out the full window: the sliced sleeper notices the flag
    // within ~100ms. The sleeper simulates the signal by flipping the
    // shared cancel flag.
    let cfg = cfg_with(5, 0, 0);
    let (prov, state) = MockProvider::new(vec![
        MockTurn::Err("503".into()),
        MockTurn::Err("not reached".into()),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("go")],
        test_ctx(),
    );
    let cancel = Arc::new(AtomicBool::new(false));
    agent.set_cancel(cancel.clone());
    let dir = std::env::temp_dir().join(format!("ta-retry-cancel-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let out = dir.join("r.json");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 2, |_| {
        cancel.store(true, Ordering::Relaxed);
    })
    .unwrap();
    assert_eq!(code, 130, "cancel during backoff → exit 130");
    assert_eq!(
        state.calls(),
        1,
        "only attempt 1 ran; cancel broke before attempt 2"
    );
    let s = std::fs::read_to_string(&out).unwrap();
    assert!(s.contains("cancelled during retry backoff"), "{s}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Simulates "user pressed stop and the stream died": flips the shared cancel
/// flag, then fails the request transiently — a 503 arriving right after a
/// Ctrl-C, the exact path the between-attempts check exists for.
struct CancelThenErrProvider {
    flag: Arc<AtomicBool>,
}
impl Provider for CancelThenErrProvider {
    fn chat(
        &self,
        _req: &ChatRequest,
        _on_event: &mut dyn FnMut(ChatEvent),
    ) -> Result<ChatResponse> {
        self.flag.store(true, Ordering::Relaxed);
        Err(anyhow::anyhow!("stream cut (503) while cancelling"))
    }
}

#[test]
fn single_run_cancel_during_failed_attempt_exits_130() {
    // Cancel-ownership contract: a stop signalled DURING a failed attempt is
    // TERMINAL for the ladder — no attempt 2 "revive". The provider flips the
    // flag then errors transiently; the between-attempts check must break
    // with exit 130 (attempts stays 1) instead of sleeping and re-running,
    // which pre-contract ran a full attempt 2 as if the user hadn't stopped.
    let cfg = cfg_with(5, 0, 0);
    let flag = Arc::new(AtomicBool::new(false));
    let prov = CancelThenErrProvider { flag: flag.clone() };
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("go")],
        test_ctx(),
    );
    agent.set_cancel(flag.clone());
    let dir = std::env::temp_dir().join(format!("ta-retry-cancel2-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let out = dir.join("r.json");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 3, |_| {}).unwrap();
    assert_eq!(code, 130, "stop during a failed attempt → exit 130");
    let s = std::fs::read_to_string(&out).unwrap();
    assert!(s.contains("cancelled during retry ladder"), "{s}");
    assert!(
        s.contains("\"attempts\": 1"),
        "attempt 2 must not start: {s}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── end-to-end harness verification: realistic multi-turn, tool-using
// tasks driven through harness::run_with_retries (the full agent loop +
// real tool execution + event flow + result.json + exit code) — the kind
// of scenario a human writes to eval the harness, not single-event units.
// All run in-process via MockProvider; retries=0 (retry behavior is pinned
// by the single_run_* tests above).

/// A tool that always returns an error result — verifies the harness
/// records tool errors (is_error=true) while the run can still succeed.
struct ErrTool;
impl Tool for ErrTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "fail".into(),
            description: "test tool that always errors".into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }
    fn run(&self, _input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        Ok(ToolOutput::error("boom: simulated failure"))
    }
}

/// Unique result.json path under temp for an E2E harness test (label
/// disambiguates parallel tests that share one pid).
fn harness_out(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("ta-e2e-{label}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    (dir.clone(), dir.join("r.json"))
}

#[test]
fn harness_tool_task_records_call_tokens_and_succeeds() {
    // The canonical eval: the model calls a tool, the harness runs it for
    // real, the model answers. result.json must carry the tool call, the
    // final summary, accumulated tokens, and success — and the tool must
    // actually have executed (not merely been recorded).
    let cfg = cfg_with(5, 0, 100_000); // big compress threshold: no compression
    let (prov, state) = MockProvider::new(vec![
        mock::resp_tool("t1", "echo", json!({"x": 1}), mock::mk_usage(50, 7)),
        mock::resp_text("the answer is 42", mock::mk_usage(30, 5)),
    ]);
    let ran = Arc::new(AtomicBool::new(false));
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(EchoTool::tracking("echo", ran.clone()))],
        vec![user_text("go")],
        test_ctx(),
    );
    let (dir, out) = harness_out("tool");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
    assert_eq!(code, 0, "recovered → exit 0");
    assert_eq!(state.calls(), 2, "two turns");
    assert!(
        ran.load(Ordering::Relaxed),
        "the tool must actually have executed"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(parsed["success"], true);
    // Attribution stamp: the bill names the model/provider that ran it
    // (cfg_with → model "m", provider openai) plus schema version.
    assert_eq!(parsed["model"], "m");
    assert_eq!(parsed["provider"], "openai");
    assert_eq!(parsed["schema_version"], 1);
    assert!(parsed["started_at"].as_str().unwrap().ends_with('Z'));
    assert!(parsed["summary"].as_str().unwrap().contains("42"));
    assert_eq!(parsed["tool_calls"].as_array().unwrap().len(), 1);
    assert_eq!(parsed["tool_calls"][0]["name"], "echo");
    assert_eq!(parsed["tool_calls"][0]["output"].as_str().unwrap(), "ok");
    assert_eq!(parsed["tool_calls"][0]["is_error"], false);
    // Tokens accumulate across both turns (50+30 ↑, 7+5 ↓).
    assert_eq!(parsed["input_tokens"], 80);
    assert_eq!(parsed["output_tokens"], 12);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn harness_multi_turn_records_each_tool_call_in_order() {
    // A run that calls two distinct tools across two turns must record both,
    // in the order they ran.
    let cfg = cfg_with(5, 0, 100_000);
    let (prov, state) = MockProvider::new(vec![
        mock::resp_tool("t1", "a", json!({}), None),
        mock::resp_tool("t2", "b", json!({}), None),
        mock::resp_text("done", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(EchoTool::new("a")), Box::new(EchoTool::new("b"))],
        vec![user_text("go")],
        test_ctx(),
    );
    let (dir, out) = harness_out("multi");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
    assert_eq!(code, 0);
    assert_eq!(state.calls(), 3, "a, b, then done");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    let names: Vec<&str> = parsed["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["a", "b"], "tool calls recorded in order");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn harness_records_a_tool_error_while_still_succeeding() {
    // A tool that errors is recorded with is_error=true; the run can still
    // succeed when the model recovers in a following turn.
    let cfg = cfg_with(5, 0, 100_000);
    let (prov, state) = MockProvider::new(vec![
        mock::resp_tool("t1", "fail", json!({}), None),
        mock::resp_text("recovered after the error", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(ErrTool)],
        vec![user_text("go")],
        test_ctx(),
    );
    let (dir, out) = harness_out("toolerr");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
    assert_eq!(code, 0, "model recovered → exit 0");
    assert_eq!(state.calls(), 2);
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(parsed["success"], true);
    assert_eq!(parsed["tool_calls"][0]["is_error"], true);
    assert!(parsed["tool_calls"][0]["output"]
        .as_str()
        .unwrap()
        .contains("boom"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn harness_max_turns_runaway_exits_nonzero() {
    // A task that never stops calling tools hits the max_turns cap: exit 1,
    // stop_reason max_turns_reached, success false. More mock tool_use turns
    // than the cap guarantees the limit — not a drained script — ends the
    // run (so stop_reason is max_turns, never a mock-exhausted "error").
    let cfg = cfg_with(2, 0, 100_000);
    let (prov, _state) = MockProvider::new(vec![
        mock::resp_tool("t1", "echo", json!({}), None),
        mock::resp_tool("t2", "echo", json!({}), None),
        mock::resp_tool("t3", "echo", json!({}), None),
        mock::resp_tool("t4", "echo", json!({}), None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(EchoTool::new("echo"))],
        vec![user_text("loop forever")],
        test_ctx(),
    );
    let (dir, out) = harness_out("maxturns");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
    assert_eq!(code, 1, "max_turns → exit 1");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(parsed["success"], false);
    assert_eq!(parsed["stop_reason"], "max_turns_reached");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn harness_json_mode_runs_clean_and_writes_result() {
    // The json=true (NDJSON) path has no end-to-end test — every other
    // harness test uses json=false. Drive a real multi-turn, tool-using run
    // through it so the per-event emit_json path (and the agent-emitted Done
    // on success) runs without panicking and result.json is still written
    // with the same contract. NDJSON line content can't be asserted from a
    // parallel test binary — its schema is locked by the event_json unit
    // tests; this guards the wiring + result file.
    let cfg = cfg_with(5, 0, 100_000);
    let (prov, _state) = MockProvider::new(vec![
        mock::resp_tool("t1", "echo", json!({"q": "hi"}), mock::mk_usage(10, 2)),
        mock::resp_text("final via ndjson", mock::mk_usage(5, 3)),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(EchoTool::new("echo"))],
        vec![user_text("go")],
        test_ctx(),
    );
    let (dir, out) = harness_out("json");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), true, 0, |_| {}).unwrap();
    assert_eq!(code, 0);
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(parsed["success"], true);
    assert_eq!(parsed["tool_calls"].as_array().unwrap().len(), 1);
    assert!(parsed["summary"].as_str().unwrap().contains("ndjson"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ── real-build integration: the agent drives its REAL write_file + shell
// tools to write an actual program, run it, and verify the outcome. The
// model side is scripted (MockProvider), but the build + verification are
// real I/O — far stronger than an EchoTool returning "ok".

#[cfg(unix)]
fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok()
}

/// Real write_file + edit_file + shell tools (not test doubles), so a
/// scripted model actually writes/edits files and runs commands. client/
/// api_key are unused by leaf tools but required by tools::build's signature.
#[cfg(unix)]
fn real_leaf_tools() -> Vec<Box<dyn Tool>> {
    let cfg = cfg_with(5, 0, 100_000);
    let client = crate::provider::new_client(60);
    crate::tools::build(
        &[
            "write_file".to_string(),
            "edit_file".to_string(),
            "shell".to_string(),
        ],
        &cfg,
        &client,
        "unused-api-key",
        &[],
    )
    .expect("build real leaf tools")
}

/// An isolated temp workdir + ToolCtx with restrict_to_workdir on, so real
/// write_file paths are confined (exercising the real confine() path too).
/// The workdir is canonicalized — confine() compares against a canonicalized
/// workdir (config load canonicalizes it), and on macOS /tmp is a symlink to
/// /private/tmp, so a non-canonical workdir would make every starts_with
/// check falsely reject the path as "escaping".
#[cfg(unix)]
fn build_ctx(label: &str) -> (ToolCtx, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("ta-build-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let canon = dir.canonicalize().unwrap();
    let ctx = ToolCtx {
        tool_absent_notes: Vec::new(),
        workdir: canon.clone(),
        max_output_chars: 10_000,
        restrict_to_workdir: true,
        cancel: None,
        shell_timeout_secs: 120,
        hooks: None,
        subagent: None,
        budget: None,
        deadline: None,
    };
    (ctx, canon)
}

#[cfg(unix)]
#[test]
fn harness_builds_and_verifies_a_python_web_service() {
    if !python3_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    // A stdlib-only HTTP server: binds a free port (0), serves one request
    // to itself, prints the body, exits — no curl, no backgrounding, no port
    // conflicts. A single `python3 svc.py` run, fully self-contained.
    let svc = r#"import http.server, socketserver, threading, urllib.request
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"Hello from TinyAgent")
    def log_message(self, *a): pass
srv = socketserver.TCPServer(("127.0.0.1", 0), H)
port = srv.server_address[1]
threading.Thread(target=srv.serve_forever, daemon=True).start()
print(urllib.request.urlopen(f"http://127.0.0.1:{port}/").read().decode())
srv.shutdown()
"#;
    let cfg = cfg_with(5, 0, 100_000);
    let (prov, _state) = MockProvider::new(vec![
        mock::resp_tool(
            "w",
            "write_file",
            json!({"path": "svc.py", "content": svc}),
            None,
        ),
        mock::resp_tool("s", "shell", json!({"command": "python3 svc.py"}), None),
        mock::resp_text("The service works: it returned Hello from TinyAgent", None),
    ]);
    let (ctx, dir) = build_ctx("websvc");
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        real_leaf_tools(),
        vec![user_text("build and verify a web service")],
        ctx,
    );
    let (rdir, out) = harness_out("websvc");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
    assert_eq!(code, 0, "build + verify run should succeed");
    // Real write_file: the file actually landed in the workdir.
    let svc_path = dir.join("svc.py");
    assert!(svc_path.exists(), "svc.py not written to workdir");
    assert!(
        std::fs::read_to_string(&svc_path)
            .unwrap()
            .contains("BaseHTTPRequestHandler"),
        "written file has the server code"
    );
    // Real shell: the service actually ran and its HTTP response was captured
    // in the shell tool's recorded output.
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(parsed["success"], true);
    let shell_out = parsed["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"].as_str() == Some("shell"))
        .expect("shell tool call recorded")["output"]
        .as_str()
        .unwrap();
    assert!(
        shell_out.contains("Hello from TinyAgent"),
        "service response should be in shell output: {shell_out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&rdir);
}

#[cfg(unix)]
#[test]
fn harness_builds_and_runs_a_python_data_pipeline() {
    if !python3_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    // A stdin word-counter: write it, pipe text through it, check the count.
    let counter = "import sys\nprint(f\"words: {len(sys.stdin.read().split())}\")\n";
    let cfg = cfg_with(5, 0, 100_000);
    let (prov, _state) = MockProvider::new(vec![
        mock::resp_tool(
            "w",
            "write_file",
            json!({"path": "count.py", "content": counter}),
            None,
        ),
        mock::resp_tool(
            "s",
            "shell",
            json!({"command": "printf 'one two three four\\n' | python3 count.py"}),
            None,
        ),
        mock::resp_text("Counted 4 words", None),
    ]);
    let (ctx, dir) = build_ctx("count");
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        real_leaf_tools(),
        vec![user_text("count the words")],
        ctx,
    );
    let (rdir, out) = harness_out("count");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
    assert_eq!(code, 0);
    assert!(dir.join("count.py").exists(), "count.py not written");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(parsed["success"], true);
    let shell_out = parsed["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"].as_str() == Some("shell"))
        .expect("shell tool call recorded")["output"]
        .as_str()
        .unwrap();
    assert!(
        shell_out.contains("words: 4"),
        "computed word count should be in output: {shell_out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&rdir);
}

/// Level up from the single-file builds: the agent writes a *multi-file*
/// project (a module + its unittest suite), runs the suite, and the real
/// shell captures unittest's "Ran 3 tests" / "OK" summary. unittest writes
/// its summary to STDERR, so it lands under the shell tool's `[stderr]`
/// section — proving stderr capture works end-to-end through the harness,
/// not just in the shell unit test. `python3 -m unittest test_calc` puts the
/// workdir on sys.path, so `from calc import ...` in the test module
/// resolves to the calc.py the agent just wrote.
#[cfg(unix)]
#[test]
fn harness_builds_a_multi_file_project_and_runs_its_test_suite() {
    if !python3_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    let calc = r#"def add(a, b):
    return a + b


def factorial(n):
    r = 1
    for i in range(2, n + 1):
        r *= i
    return r


def is_prime(n):
    if n < 2:
        return False
    for i in range(2, int(n**0.5) + 1):
        if n % i == 0:
            return False
    return True
"#;
    let test_calc = r#"import unittest
from calc import add, factorial, is_prime


class TestCalc(unittest.TestCase):
    def test_add(self):
        self.assertEqual(add(2, 3), 5)

    def test_factorial(self):
        self.assertEqual(factorial(5), 120)

    def test_is_prime(self):
        self.assertTrue(is_prime(7))
        self.assertFalse(is_prime(8))


if __name__ == "__main__":
    unittest.main()
"#;
    let cfg = cfg_with(8, 0, 100_000);
    let (prov, _state) = MockProvider::new(vec![
        mock::resp_tool(
            "w1",
            "write_file",
            json!({"path": "calc.py", "content": calc}),
            None,
        ),
        mock::resp_tool(
            "w2",
            "write_file",
            json!({"path": "test_calc.py", "content": test_calc}),
            None,
        ),
        mock::resp_tool(
            "s",
            "shell",
            json!({"command": "python3 -m unittest test_calc"}),
            None,
        ),
        mock::resp_text("All 3 tests passed", None),
    ]);
    let (ctx, dir) = build_ctx("multifile");
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        real_leaf_tools(),
        vec![user_text("build a calc module with tests and run them")],
        ctx,
    );
    let (rdir, out) = harness_out("multifile");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
    assert_eq!(code, 0, "multi-file build + test run should succeed");
    assert!(dir.join("calc.py").exists(), "calc.py not written");
    assert!(
        dir.join("test_calc.py").exists(),
        "test_calc.py not written"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(parsed["success"], true);
    let shell_out = parsed["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"].as_str() == Some("shell"))
        .expect("shell tool call recorded")["output"]
        .as_str()
        .unwrap();
    // unittest prints its summary to stderr → captured under [stderr].
    assert!(
        shell_out.contains("Ran 3 tests"),
        "unittest 'Ran 3 tests' summary (on stderr) should be captured: {shell_out}"
    );
    assert!(
        shell_out.contains("OK"),
        "unittest 'OK' result line should be captured: {shell_out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&rdir);
}

/// The iterative debug loop: write a *buggy* program, run it and observe
/// the wrong output, edit_file the one-line fix, re-run and observe the
/// right output. This is the first test to exercise the real edit_file
/// tool end-to-end through the harness (not just its unit tests) AND to
/// assert tool_calls are recorded in execution order across turns — the
/// first shell prints 24 (factorial(5) with an off-by-one `range(2, n)`),
/// the second prints 120 (after `range(2, n + 1)`). It also guards a real
/// footgun: real_leaf_tools() must register edit_file, or the tool_result
/// silently becomes "unknown tool: edit_file" and the run still "succeeds"
/// without ever fixing the bug.
#[cfg(unix)]
#[test]
fn harness_edits_a_bug_and_re_verifies_the_fix() {
    if !python3_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    // Buggy factorial: range(2, n) stops one short, so 5! = 2*3*4 = 24.
    let buggy = r#"import sys


def factorial(n):
    r = 1
    for i in range(2, n):
        r *= i
    return r


print(factorial(int(sys.argv[1])))
"#;
    let cfg = cfg_with(8, 0, 100_000);
    let (prov, _state) = MockProvider::new(vec![
        mock::resp_tool(
            "w",
            "write_file",
            json!({"path": "prog.py", "content": buggy}),
            None,
        ),
        mock::resp_tool("s1", "shell", json!({"command": "python3 prog.py 5"}), None),
        mock::resp_tool(
            "e",
            "edit_file",
            json!({"path": "prog.py", "old_string": "range(2, n)", "new_string": "range(2, n + 1)"}),
            None,
        ),
        mock::resp_tool("s2", "shell", json!({"command": "python3 prog.py 5"}), None),
        mock::resp_text("Fixed: 5! = 120", None),
    ]);
    let (ctx, dir) = build_ctx("editbug");
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        real_leaf_tools(),
        vec![user_text("find the factorial bug, fix it, re-verify")],
        ctx,
    );
    let (rdir, out) = harness_out("editbug");
    let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
    assert_eq!(code, 0, "edit-and-reverify run should succeed");
    // The real edit_file mutated the file on disk: the fix is present, the
    // buggy line is gone.
    let patched = std::fs::read_to_string(dir.join("prog.py")).unwrap();
    assert!(
        patched.contains("range(2, n + 1)"),
        "patched file should contain the fix: {patched}"
    );
    assert!(
        !patched.contains("for i in range(2, n):"),
        "buggy line should be gone after edit: {patched}"
    );
    // tool_calls are in execution order: first shell = buggy 24, second =
    // fixed 120. (filter, not find: there are two shell calls this time.)
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(parsed["success"], true);
    let shell_runs: Vec<&str> = parsed["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["name"].as_str() == Some("shell"))
        .map(|c| c["output"].as_str().unwrap())
        .collect();
    assert_eq!(
        shell_runs.len(),
        2,
        "expected two shell runs (buggy then fixed)"
    );
    assert!(
        shell_runs[0].contains("24"),
        "first (buggy) run should print 24: {}",
        shell_runs[0]
    );
    assert!(
        shell_runs[1].contains("120"),
        "second (fixed) run should print 120: {}",
        shell_runs[1]
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&rdir);
}
