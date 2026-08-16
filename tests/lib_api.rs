//! Library API smoke test: drives an agent loop end-to-end through the public
//! `tiny_agent` API with no binary, no network, and no API key — a scripted
//! mock provider plus a custom `CallbackTool`. Gated on the `mock` feature.
#![cfg(feature = "mock")]

use serde_json::json;
use tiny_agent::provider::mock::{mk_usage, resp_text, resp_tool, MockProvider};
use tiny_agent::{Agent, AgentEvent, CallbackTool, Config, Provider, ToolCtx, ToolDef, ToolOutput};

fn minimal_config() -> Config {
    // Config has no Default, but it derives Deserialize — build the smallest
    // valid one inline (every other field has a serde default).
    serde_json::from_value(json!({
        "provider": "openai",
        "api_key_env": "NONE",
        "model": "mock-model",
        "tools": [],
        "inject_environment": false
    }))
    .expect("minimal config")
}

#[test]
fn custom_callback_tool_runs_and_usage_is_honest() {
    // Script the provider: turn 1 → model calls our "echo" tool;
    // turn 2 → model ends the run with plain text. Each carries token usage so
    // we can assert the library's honest cost accumulation.
    let (provider, _state) = MockProvider::new(vec![
        resp_tool("call_1", "echo", json!({"msg": "hello"}), mk_usage(10, 5)),
        resp_text("all done", mk_usage(8, 3)),
    ]);
    let provider: Box<dyn Provider> = Box::new(provider);

    // A custom tool built from a plain closure via CallbackTool — this is the
    // exact path the Python / TypeScript bindings will take (their callable is
    // wrapped in a closure that handles GIL / threadsafe-function, then handed
    // here so the Tool impl is shared).
    let echo = CallbackTool::new(
        ToolDef {
            name: "echo".into(),
            description: "echo the message back".into(),
            schema: json!({
                "type": "object",
                "properties": {"msg": {"type": "string"}},
                "required": ["msg"]
            }),
        },
        |input, _ctx| {
            Ok(ToolOutput::text(format!(
                "echo: {}",
                input["msg"].as_str().unwrap_or("")
            )))
        },
    );

    let cfg = minimal_config();
    let ctx = ToolCtx {
        workdir: std::env::current_dir().unwrap(),
        max_output_chars: cfg.limits.max_output_chars,
        restrict_to_workdir: false,
        cancel: None,

        shell_timeout_secs: 120,
    };
    let mut agent = Agent::new(&cfg, provider, vec![Box::new(echo)], Vec::new(), ctx);

    let mut saw_toolcall = false;
    let mut saw_toolresult = false;
    let mut texts = String::new();
    let outcome = agent
        .run(&mut |ev| match ev {
            AgentEvent::ToolCall { name, .. } => {
                saw_toolcall = true;
                assert_eq!(name, "echo");
            }
            AgentEvent::ToolResult { is_error, content, .. } => {
                saw_toolresult = true;
                assert!(!is_error, "tool result should not be an error");
                assert!(content.contains("echo: hello"), "content was: {content}");
            }
            AgentEvent::TextDelta(t) => texts.push_str(&t),
            _ => {}
        })
        .expect("run succeeds");

    // The custom tool was actually invoked and its result reached the stream.
    assert!(saw_toolcall, "ToolCall event never fired");
    assert!(saw_toolresult, "ToolResult event never fired");
    assert!(texts.contains("all done"), "final text missing: {texts}");

    // Two provider turns ran; the run ended cleanly on end_turn.
    assert_eq!(outcome.turns, 2);
    assert_eq!(outcome.stop_reason, "end_turn");

    // Honest cost: both turns' input/output tokens accumulated into the outcome.
    assert_eq!(outcome.input_tokens, 18); // 10 + 8
    assert_eq!(outcome.output_tokens, 8); // 5 + 3
    assert_eq!(outcome.tool_calls.len(), 1);
    assert_eq!(outcome.tool_calls[0].name, "echo");

    // The post-run getters mirror the outcome (library users read state this way).
    assert_eq!(agent.usage().input_tokens, 18);
    assert_eq!(agent.usage().output_tokens, 8);
    assert_eq!(agent.model(), "mock-model");
    assert!(!agent.messages().is_empty(), "history should hold the turns");
}

#[test]
fn agent_event_is_serializable_for_ffi() {
    // The Python / TypeScript bindings hand each AgentEvent to host code as a
    // native dict / object via serde, so the type must be Serialize (including
    // the recursive SubAgent variant). A round-trip through JSON proves it.
    let ev = AgentEvent::ToolCall {
        name: "echo".into(),
        input: json!({"msg": "hi"}),
    };
    let s = serde_json::to_string(&ev).expect("serialize");
    assert!(s.contains("echo"), "serialized: {s}");

    // The recursive SubAgent variant must serialize too.
    let inner = AgentEvent::TextDelta("child".into());
    let sub = AgentEvent::SubAgent { name: "worker".into(), event: Box::new(inner) };
    let s2 = serde_json::to_string(&sub).expect("serialize subagent");
    assert!(s2.contains("worker") && s2.contains("child"), "serialized: {s2}");
}

#[test]
fn build_agent_with_provider_assembles_and_runs() {
    // Cover the build_agent* facade (assemble: tool wiring + workdir + ToolCtx),
    // which custom_callback_tool_runs_and_usage_is_honest bypasses by calling
    // Agent::new directly. Using build_agent_with_provider injects a mock
    // provider and asserts the assembled agent runs end-to-end with an extra
    // custom tool appended.
    use tiny_agent::provider::new_client;
    let cfg = minimal_config();
    let client = new_client(0);
    let (provider, _state) =
        MockProvider::new(vec![resp_text("ok from facade", mk_usage(3, 2))]);
    let mut agent = tiny_agent::build_agent_with_provider(
        &cfg,
        Box::new(provider),
        Vec::new(),
        &client,
        Vec::new(),
        "",
    )
    .expect("build_agent_with_provider assembles");
    let mut texts = String::new();
    let outcome = agent
        .run(&mut |ev| {
            if let AgentEvent::TextDelta(t) = ev {
                texts.push_str(&t);
            }
        })
        .expect("facade-assembled agent runs");
    assert_eq!(outcome.stop_reason, "end_turn");
    assert!(texts.contains("ok from facade"), "ran via facade: {texts}");
    assert_eq!(outcome.input_tokens, 3);
}

#[test]
fn run_error_serializes_structured_cost() {
    // The bindings serialize RunError to surface a failed run's burned tokens /
    // tool calls / turns to the host (CLAUDE.md "重试诚实成本"). A manual
    // Serialize impl carries usage/tool_calls/turns/elapsed and renders source
    // (anyhow::Error, not auto-Serializable) as its Display string under `error`.
    // Drive a real failing run (provider Err) so the RunError is authentic.
    use tiny_agent::provider::mock;
    let cfg = minimal_config();
    let (provider, _) = MockProvider::new(vec![
        // turn 1 bills some usage then…
        resp_text("partial", mk_usage(100, 10)),
    ]);
    // Re-script to fail: a provider Err surfaces as RunError carrying the
    // accumulated cost. Use a one-turn script that errors immediately.
    let (provider, _) = MockProvider::new(vec![mock::MockTurn::Err("boom: 503".into())]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(provider),
        vec![],
        vec![tiny_agent::Message::user(vec![tiny_agent::Block::Text("go".into())])],
        ToolCtx {
            workdir: std::env::current_dir().unwrap(),
            max_output_chars: 4000,
            restrict_to_workdir: false,
            cancel: None,
        },
    );
    let re = match agent.run(&mut |_| {}) {
        Err(e) => e,
        Ok(_) => panic!("expected a provider error"),
    };
    let v = serde_json::to_value(&re).expect("serialize RunError");
    assert_eq!(v["error"], "boom: 503", "source renders as its Display string");
    assert!(v["usage"].is_object(), "usage serializes");
    assert!(v["turns"].is_number(), "turns serializes as a number");
    assert!(v["elapsed"].is_object(), "Duration serializes (serde json)");
}
