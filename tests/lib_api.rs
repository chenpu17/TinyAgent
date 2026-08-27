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
        hooks: None,
        subagent: None,
        budget: None,
        deadline: None,
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
    // Drive a real failing run (provider Err) so the RunError is authentic:
    // a one-turn script that errors immediately.
    use tiny_agent::provider::mock;
    let cfg = minimal_config();
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
            shell_timeout_secs: 120,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
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

/// A one-off custom tool with a caller-chosen name, for the name-validation
/// tests below (the exact path a binding's callable takes into assemble).
fn plain_tool(name: &str) -> CallbackTool {
    CallbackTool::new(
        ToolDef {
            name: name.into(),
            description: "validation probe".into(),
            schema: json!({"type": "object", "properties": {}}),
        },
        |_input, _ctx| Ok(ToolOutput::text("ok")),
    )
}

#[test]
fn duplicate_tool_name_fails_at_build() {
    // A custom tool named like a built-in used to be silently SHADOWED
    // (dispatch resolves by first match) while the model saw two definitions
    // of the same name — providers reject that with a 400 mid-run. Startup
    // must fail instead, naming the collision.
    let mut cfg = minimal_config();
    cfg.tools = vec!["shell".into()];
    let client = reqwest::blocking::Client::new();
    let err = tiny_agent::build_agent_with(
        &cfg,
        vec![],
        &client,
        vec![Box::new(plain_tool("shell"))],
        "k",
    );
    let err = match err {
        Err(e) => e,
        Ok(_) => panic!("built-in vs custom collision must fail at build"),
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("duplicate"), "{msg}");
    assert!(msg.contains("shell"), "the colliding name is named: {msg}");
    // Two custom tools colliding with each other hit the same check.
    let err = tiny_agent::build_agent_with(
        &cfg,
        vec![],
        &client,
        vec![Box::new(plain_tool("dup")), Box::new(plain_tool("dup"))],
        "k",
    );
    let err = match err {
        Err(e) => e,
        Ok(_) => panic!("custom vs custom collision must fail at build"),
    };
    assert!(format!("{err:#}").contains("duplicate"));
}

#[test]
fn invalid_tool_name_fails_at_build() {
    // Charset and length are the Anthropic tool-name / OpenAI function-name
    // wire limits; violating them used to surface as a provider 400 on the
    // first request instead of a build error.
    let cfg = minimal_config();
    let client = reqwest::blocking::Client::new();
    for bad in ["my tool", "echo!", "工具", "a".repeat(65).as_str()] {
        let err = tiny_agent::build_agent_with(
            &cfg,
            vec![],
            &client,
            vec![Box::new(plain_tool(bad))],
            "k",
        );
        let err = match err {
            Err(e) => e,
            Ok(_) => panic!("'{bad}' must be rejected at build"),
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("tool name") || msg.contains("tool names"),
            "message names the offending tool: {msg}"
        );
    }
    // The boundary itself passes: exactly 64 chars of legal characters.
    tiny_agent::build_agent_with(
        &cfg,
        vec![],
        &client,
        vec![Box::new(plain_tool(&"a".repeat(64)))],
        "k",
    )
    .expect("64-char name is within the provider limit");
}
