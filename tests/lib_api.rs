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
            AgentEvent::ToolResult {
                is_error, content, ..
            } => {
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
    assert!(
        !agent.messages().is_empty(),
        "history should hold the turns"
    );
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
    let sub = AgentEvent::SubAgent {
        name: "worker".into(),
        event: Box::new(inner),
    };
    let s2 = serde_json::to_string(&sub).expect("serialize subagent");
    assert!(
        s2.contains("worker") && s2.contains("child"),
        "serialized: {s2}"
    );
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
    let (provider, _state) = MockProvider::new(vec![resp_text("ok from facade", mk_usage(3, 2))]);
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
        vec![tiny_agent::Message::user(vec![tiny_agent::Block::Text(
            "go".into(),
        )])],
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
    assert_eq!(
        v["error"], "boom: 503",
        "source renders as its Display string"
    );
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

fn plain_ctx() -> ToolCtx {
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
    }
}

#[test]
fn try_new_rejects_an_invalid_tool_set_as_an_error() {
    // Direct construction is the README's "full control" path; validation used
    // to live only in assembly, so a hand-built list with duplicates (or
    // illegal names) reintroduced silent shadowing / provider 400s. try_new
    // surfaces the same validate_tool_names contract as the builders.
    let cfg = minimal_config();
    let (provider, _) = MockProvider::new(vec![]);
    let tools: Vec<Box<dyn tiny_agent::Tool>> =
        vec![Box::new(plain_tool("dup")), Box::new(plain_tool("dup"))];
    let err = tiny_agent::Agent::try_new(&cfg, Box::new(provider), tools, Vec::new(), plain_ctx());
    let err = match err {
        Err(e) => e,
        Ok(_) => panic!("duplicate names must fail try_new"),
    };
    assert!(format!("{err:#}").contains("duplicate"), "{err:#}");

    let (provider, _) = MockProvider::new(vec![]);
    let bad: Vec<Box<dyn tiny_agent::Tool>> = vec![Box::new(plain_tool("no good"))];
    let err = tiny_agent::Agent::try_new(&cfg, Box::new(provider), bad, Vec::new(), plain_ctx());
    let err = match err {
        Err(e) => e,
        Ok(_) => panic!("illegal name must fail try_new"),
    };
    assert!(format!("{err:#}").contains("tool name"), "{err:#}");

    // A valid set constructs fine.
    let (provider, _) = MockProvider::new(vec![]);
    let ok: Vec<Box<dyn tiny_agent::Tool>> = vec![Box::new(plain_tool("fine"))];
    tiny_agent::Agent::try_new(&cfg, Box::new(provider), ok, Vec::new(), plain_ctx())
        .expect("valid tool set constructs");
}

#[test]
#[should_panic(expected = "invalid tool set")]
fn new_panics_on_an_invalid_tool_set() {
    // Agent::new keeps its infallible signature (dozens of internal call
    // sites); an invalid set is a construction-time programmer error, and the
    // panic message points at try_new for the recoverable path.
    let cfg = minimal_config();
    let (provider, _) = MockProvider::new(vec![]);
    let dup: Vec<Box<dyn tiny_agent::Tool>> =
        vec![Box::new(plain_tool("dup")), Box::new(plain_tool("dup"))];
    let _ = tiny_agent::Agent::new(&cfg, Box::new(provider), dup, Vec::new(), plain_ctx());
}

// ── FR-1 skill_injection (finetune_support_design) — code feature only ─────

#[cfg(feature = "code")]
fn fr1_scratch(tag: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "ta-lib-fr1-{}-{tag}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// code.enabled + a skill_injection policy, in an isolated workdir, with an
/// output cap large enough that serving never truncates (byte-identity
/// assertions below compare against the full shipped skill md).
#[cfg(feature = "code")]
fn fr1_config(skill_injection: &str, workdir: &std::path::Path) -> Config {
    serde_json::from_value(json!({
        "provider": "openai",
        "api_key_env": "NONE",
        "model": "mock-model",
        "tools": [],
        "inject_environment": false,
        "workdir": workdir.to_str().unwrap(),
        "limits": {"max_output_chars": 4000000},
        "code": {"enabled": true, "skill_injection": skill_injection}
    }))
    .expect("fr1 config")
}

/// A-1 eager: a fresh session starts with the synthetic read_skill pair —
/// byte-identical content to the shipped skill md (the same bytes read_skill
/// would serve: they share `serve_skill_body`), N:N paired, ids minted by the
/// ingest point.
#[cfg(feature = "code")]
#[test]
fn eager_skill_injection_prepends_synthetic_pair() {
    use tiny_agent::provider::new_client;
    let dir = fr1_scratch("eager");
    let cfg = fr1_config("eager", &dir);
    let client = new_client(0);
    let (provider, _s) = MockProvider::new(vec![]);
    let agent = tiny_agent::build_agent_with_provider(
        &cfg,
        Box::new(provider),
        Vec::new(),
        &client,
        vec![],
        "",
    )
    .expect("assemble eager");
    let msgs = agent.messages();
    assert_eq!(msgs.len(), 2, "synthetic pair only, got: {msgs:?}");
    assert_eq!(msgs[0].role, tiny_agent::Role::Assistant);
    match &msgs[0].content[0] {
        tiny_agent::Block::ToolUse { id, name, input } => {
            assert_eq!(id, "eager-run-code");
            assert_eq!(name, "read_skill");
            assert_eq!(input, &json!({"skill": "run-code"}));
        }
        other => panic!("expected tool_use, got {other:?}"),
    }
    assert_eq!(msgs[1].role, tiny_agent::Role::User);
    // code_memory MD-13: the eager pair carries the EFFECTIVE skill body for
    // this assembly — include off (the default) strips the marked lib
    // section, so the injected bytes are the gated file, not the raw one.
    let md = tiny_agent::tools::run_code::run_code_skill_md(false, false, false);
    match &msgs[1].content[0] {
        tiny_agent::Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => {
            assert_eq!(tool_use_id, "eager-run-code", "N:N paired");
            assert!(!is_error);
            assert_eq!(content, &md, "byte-identical to the effective skill md");
        }
        other => panic!("expected tool_result, got {other:?}"),
    }
    assert!(
        msgs[0].id.is_some() && msgs[1].id.is_some() && msgs[0].id != msgs[1].id,
        "ids minted by the ingest point: {:?} / {:?}",
        msgs[0].id,
        msgs[1].id
    );
}

/// A-1 eager + headless 形状（e2e 修订回归）：`main.rs` / `batch.rs` 把任务
/// 消息放进 initial——这不是 resume。对要前置，且边界合并（对尾的
/// user(tool_result) 与任务 user 合成一条，append_user_turn 规则——相邻
/// user 在 Anthropic 400；合并后的形状与绑定/TUI 路径 run() 时产生的完全
/// 一致）。
#[cfg(feature = "code")]
#[test]
fn eager_injection_prepends_before_headless_task_message() {
    use tiny_agent::provider::new_client;
    let dir = fr1_scratch("headless");
    let cfg = fr1_config("eager", &dir);
    let client = new_client(0);
    let (provider, _s) = MockProvider::new(vec![]);
    let initial = vec![tiny_agent::Message::user(vec![tiny_agent::Block::Text(
        "用 run_code 计算 1 到 100 的和".into(),
    )])];
    let agent = tiny_agent::build_agent_with_provider(
        &cfg,
        Box::new(provider),
        initial,
        &client,
        vec![],
        "",
    )
    .expect("assemble eager over headless task");
    let msgs = agent.messages();
    assert_eq!(msgs.len(), 2, "pair + merged task: {msgs:?}");
    assert_eq!(msgs[0].role, tiny_agent::Role::Assistant);
    assert!(
        matches!(&msgs[0].content[0], tiny_agent::Block::ToolUse { name, .. } if name == "read_skill")
    );
    // 边界合并成单条 user：tool_result 在前、任务文本在后，无相邻 user。
    assert_eq!(msgs[1].role, tiny_agent::Role::User);
    match (&msgs[1].content[0], &msgs[1].content[1..]) {
        (tiny_agent::Block::ToolResult { tool_use_id, .. }, [tiny_agent::Block::Text(task)]) => {
            assert_eq!(tool_use_id, "eager-run-code");
            assert_eq!(task, "用 run_code 计算 1 到 100 的和");
        }
        other => panic!("expected [tool_result, text] in ONE user message, got {other:?}"),
    }
}

/// A-1 eager + 已披露历史不重注（FD-3 内容判定，e2e 修订）：历史里已有
/// `read_skill("run-code")` 交换（eager 会话 resume 的合成对、或真实调用）
/// 时正文已在历史中，再注入即重复——原样保留。
#[cfg(feature = "code")]
#[test]
fn eager_injection_skips_already_disclosed_history() {
    use tiny_agent::provider::new_client;
    let dir = fr1_scratch("resume");
    let cfg = fr1_config("eager", &dir);
    let client = new_client(0);
    let (provider, _s) = MockProvider::new(vec![]);
    let initial = vec![
        tiny_agent::Message::user(vec![tiny_agent::Block::Text("resumed task".into())]),
        tiny_agent::Message::assistant(vec![tiny_agent::Block::ToolUse {
            id: "old-call".into(),
            name: "read_skill".into(),
            input: json!({"skill": "run-code"}),
        }]),
        tiny_agent::Message::user(vec![tiny_agent::Block::ToolResult {
            tool_use_id: "old-call".into(),
            content: "(the previously served body)".into(),
            image: None,
            is_error: false,
        }]),
    ];
    let agent = tiny_agent::build_agent_with_provider(
        &cfg,
        Box::new(provider),
        initial,
        &client,
        vec![],
        "",
    )
    .expect("assemble eager over disclosed history");
    let msgs = agent.messages();
    assert_eq!(msgs.len(), 3, "no re-injection: {msgs:?}");
    assert_eq!(msgs[1].content.len(), 1, "history untouched");
}

/// A-1 on_demand (default): no injection — history starts empty.
#[cfg(feature = "code")]
#[test]
fn on_demand_injects_nothing() {
    use tiny_agent::provider::new_client;
    let dir = fr1_scratch("ondemand");
    let cfg = fr1_config("on_demand", &dir);
    let client = new_client(0);
    let (provider, _s) = MockProvider::new(vec![]);
    let agent = tiny_agent::build_agent_with_provider(
        &cfg,
        Box::new(provider),
        Vec::new(),
        &client,
        vec![],
        "",
    )
    .expect("assemble on_demand");
    assert!(agent.messages().is_empty());
}

/// A-1 off: with another disk skill keeping read_skill registered, asking for
/// run-code yields the tombstone error (附录 A-0, verbatim) through the real
/// loop; and nothing is injected into history.
#[cfg(feature = "code")]
#[test]
fn off_skill_injection_tombstones_read_skill() {
    use tiny_agent::provider::new_client;
    let dir = fr1_scratch("off");
    let sk = dir.join(".tinyagent").join("skills").join("probe");
    std::fs::create_dir_all(&sk).unwrap();
    std::fs::write(
        sk.join("SKILL.md"),
        "---\nname: probe\ndescription: p\n---\nbody\n",
    )
    .unwrap();
    let cfg = fr1_config("off", &dir);
    let client = new_client(0);
    let (provider, _s) = MockProvider::new(vec![
        resp_tool(
            "t1",
            "read_skill",
            json!({"skill": "run-code"}),
            mk_usage(5, 1),
        ),
        resp_text("done", mk_usage(3, 1)),
    ]);
    let mut agent = tiny_agent::build_agent_with_provider(
        &cfg,
        Box::new(provider),
        Vec::new(),
        &client,
        vec![],
        "",
    )
    .expect("assemble off");
    assert!(agent.messages().is_empty(), "off injects nothing");
    let outcome = agent.run(&mut |_| {}).expect("run with tombstone");
    assert_eq!(outcome.tool_calls.len(), 1);
    assert!(outcome.tool_calls[0].is_error);
    assert_eq!(
        outcome.tool_calls[0].output,
        "read_skill: skill \"run-code\" 已被配置关闭（code.skill_injection = \"off\"）。run_code 的环境事实见其工具描述；完整用法文档在此配置下不可用。"
    );
}

/// Isolated temp workdir for the context-attribution tests (fr1_scratch is
/// `code`-gated; these tests are not).
fn ctx_scratch(tag: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "ta-lib-ctx-{}-{tag}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `context_breakdown`'s assembly-time attribution: the skill intro
/// `assemble` appends to the system prompt is its own segment and the
/// user's base keeps its pre-assembly share; the skill tools land in the
/// skill bucket. A workdir without skills gets no intro segment.
#[test]
fn build_agent_skill_intro_attribution() {
    use tiny_agent::provider::new_client;
    let dir = ctx_scratch("attrib");
    let skill_dir = dir.join(".tinyagent/skills/demo-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: a demo skill for attribution\n---\n\nBody text.",
    )
    .unwrap();
    let cfg: Config = serde_json::from_value(json!({
        "provider": "openai",
        "api_key_env": "NONE",
        "model": "mock-model",
        "tools": [],
        "system_prompt": "You are the base prompt.",
        "workdir": dir.to_str().unwrap()
    }))
    .expect("config");
    let client = new_client(0);
    let (provider, _s) = MockProvider::new(vec![]);
    let agent = tiny_agent::build_agent_with_provider(
        &cfg,
        Box::new(provider),
        Vec::new(),
        &client,
        vec![],
        "",
    )
    .expect("assemble with a skill");
    let bd = agent.context_breakdown();
    assert!(bd.system.skill_intro > 0, "intro appended and attributed");
    assert!(bd.system.base > 0, "user base prompt attributed");
    assert_eq!(
        bd.system.env + bd.system.base + bd.system.skill_intro,
        bd.system.tokens,
        "three-way partition"
    );
    let names: Vec<&str> = bd
        .tools
        .skill
        .tools
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(names, ["read_skill", "list_skill"]);

    // The "no skills anywhere → skill_intro == 0" leg is NOT asserted here:
    // `resolve_skill_dirs` also reads the machine's GLOBAL skills dir, so an
    // empty workdir still sees the developer's installed skills — the claim
    // is environment-dependent. The no-intro default (direct `Agent::new`,
    // no assembly) is pinned in agent_tests::env_attribution instead.
}

/// The bindings hand the breakdown to host code as a native dict / object
/// via serde — pin the JSON face (snake_case keys, `calibration` null
/// before any billed run).
#[test]
fn context_breakdown_serializable() {
    let cfg = minimal_config();
    let agent = Agent::new(
        &cfg,
        Box::new(MockProvider::new(vec![]).0),
        vec![],
        vec![tiny_agent::Message::user(vec![tiny_agent::Block::Text(
            "hello".into(),
        )])],
        plain_ctx(),
    );
    let v = serde_json::to_value(agent.context_breakdown()).unwrap();
    for k in ["system", "tools", "messages", "total_tokens", "calibration"] {
        assert!(v.get(k).is_some(), "missing top-level key {k}");
    }
    assert!(v["calibration"].is_null(), "no billed run yet");
    for k in ["env", "base", "skill_intro", "tokens"] {
        assert!(v["system"].get(k).is_some(), "missing system.{k}");
    }
    for k in ["builtin", "mcp", "skill", "custom", "tokens"] {
        assert!(v["tools"].get(k).is_some(), "missing tools.{k}");
    }
    for k in [
        "user_text",
        "assistant_text",
        "tool_use",
        "tool_result",
        "image",
        "document",
        "skill_content",
        "other",
    ] {
        assert!(
            v["messages"]["by_kind"].get(k).is_some(),
            "missing by_kind.{k}"
        );
    }
}

// ── code_llm M3: A-L9 — the llm primitive rides the OUTER provider instance
// (LD-16), end to end through the real assembly fill path.
#[cfg(all(feature = "code", feature = "mock"))]
#[test]
fn llm_primitive_rides_the_outer_provider_instance() {
    use tiny_agent::provider::new_client;
    // Strict model routing (M0 topology): the outer model, the tier's m1/m2,
    // and m3 — allowlisted but deliberately UNROUTED, the "custom provider
    // cannot serve that model" honest-failure shape.
    let outer_code = r#"
        const one = tools.call("llm", {prompt: "first"});
        const two = tools.call("llm", {prompt: "second", model: "m2", max_tokens: 32});
        let third;
        try {
            tools.call("llm", {prompt: "third", model: "m3"});
            third = "no-throw";
        } catch (e) { third = e.kind; }
        return { one: one.text, two: two.text, m2: two.model, third };"#;
    let (provider, state) = MockProvider::routed(vec![
        (
            "mock-model",
            vec![
                resp_tool(
                    "t1",
                    "run_code",
                    json!({"code": outer_code}),
                    mk_usage(20, 10),
                ),
                resp_text("done", mk_usage(5, 5)),
            ],
        ),
        ("m1", vec![resp_text("answer-one", mk_usage(7, 3))]),
        ("m2", vec![resp_text("answer-two", mk_usage(9, 4))]),
    ]);
    let cfg: Config = serde_json::from_value(json!({
        "provider": "openai",
        "api_key_env": "NONE",
        "model": "mock-model",
        "tools": [],
        "inject_environment": false,
        "limits": {"max_turns": 5, "timeout_secs": 120, "max_total_tokens": 100000},
        "code": {
            "enabled": true,
            "llm": {"enabled": true, "default_model": "m1", "models": ["m1", "m2", "m3"]}
        }
    }))
    .expect("config");
    let client = new_client(0);
    let mut agent = tiny_agent::build_agent_with_provider(
        &cfg,
        Box::new(provider),
        vec![tiny_agent::Message::user(vec![tiny_agent::Block::Text(
            "go".into(),
        )])],
        &client,
        Vec::new(),
        "",
    )
    .expect("assemble");
    let outcome = agent.run(&mut |_| {}).expect("run");
    assert_eq!(outcome.stop_reason, "end_turn");
    // Same-instance proof: ONE MockState saw the outer turns AND the inner
    // model calls, in dispatch order — the llm cell carries the very Arc the
    // outer agent chats through (the LD-16 fill in Agent::set_code_tool).
    let captured = state.captured();
    let models: Vec<&str> = captured.iter().map(|c| c.model.as_str()).collect();
    assert_eq!(
        models,
        vec!["mock-model", "m1", "m2", "m3", "mock-model"],
        "outer turn → inner m1/m2 (allowlist) → m3 (unrouted, errors) → outer final"
    );
    assert_eq!(captured[1].temperature, Some(0.0), "LD-4 on the inner call");
    assert_eq!(captured[2].max_tokens, 32, "explicit max_tokens forwarded");
    assert!(!captured[1].cache, "one-shot, no cache markup");
    // The guest saw the texts + the honest m3 failure class — they travel in
    // the run_code result record the outer model received.
    let rc = outcome
        .tool_calls
        .iter()
        .find(|r| r.name == "run_code")
        .expect("run_code record");
    assert!(
        rc.output.contains("answer-one") && rc.output.contains("answer-two"),
        "guest saw both routed answers: {}",
        rc.output
    );
    assert!(
        rc.output.contains("llm"),
        "the unrouted m3 surfaced as kind=llm: {}",
        rc.output
    );
    assert!(!rc.is_error);
    let llm = rc
        .meta
        .as_ref()
        .expect("run_code meta")
        .get("llm")
        .expect("LD-12 ledger");
    assert_eq!(llm["calls"], json!(3));
    assert_eq!(llm["models"]["m1"]["known_usage"], json!(10));
    assert_eq!(llm["models"]["m2"]["known_usage"], json!(13));
    assert_eq!(llm["models"]["m3"]["unknown_calls"], json!(1));
    // The inner spend folded ONCE into the run totals: outer turns 20+10 and
    // 5+5, inner 16+7 (m1 7+3, m2 9+4, m3 nothing — unknown stays unknown,
    // never faked as zero-cost usage).
    assert_eq!(outcome.input_tokens, 41);
    assert_eq!(outcome.output_tokens, 22);
}
