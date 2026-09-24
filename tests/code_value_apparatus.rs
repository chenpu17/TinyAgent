//! code_value G0 装置：A/B 工具清单与提示词差分导出（run_code 价值验证
//! v0.3 方案 §3 / 工单 01 第 4 条）。
//!
//! 由 scripts/code_value_eval.py 通过环境变量驱动：
//!   CODE_VALUE_INVENTORY_SPEC = JSON 文件，列出每个待导出的 config
//!     {"out": "<输出路径>", "configs": [{"key": "t0-A", "path": "...",
//!       "task": "t0", "arm": "A"}, ...]}
//! 未设该变量时本测试是 no-op（CI 的常规 cargo test 不受影响）。
//!
//! 导出内容（不是配置白名单字符串的转抄——全部取自真实构造的 Agent）：
//!   - agent_tool_names / tool_defs：Agent::tool_names()/tool_defs()，
//!     即每次请求真正序列化给模型的工具表；
//!   - request_tool_names：MockProvider 实捕的请求侧工具名（wire 证据）；
//!   - system_prompt：装配后的完整系统提示词（env 头 + base + skill intro）；
//!   - guest_tools：A 臂经真实 run_code 调用 `return tools.list()` 取回的
//!     guest 可调用工具（沙箱 registry 的运行时真值）；B 臂为 null。
#![cfg(feature = "mock")]

use serde_json::json;
use std::path::PathBuf;
use tiny_agent::provider::mock::{mk_usage, resp_text, resp_tool, MockProvider};
use tiny_agent::provider::new_client;

#[test]
fn dump_ab_inventory() {
    let spec_path = match std::env::var("CODE_VALUE_INVENTORY_SPEC") {
        Ok(v) => PathBuf::from(v),
        Err(_) => return, // 常规测试运行：装置外，no-op
    };
    let spec: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&spec_path).unwrap()).expect("spec json");
    let mut out = serde_json::Map::new();

    for entry in spec["configs"].as_array().expect("configs array") {
        let key = entry["key"].as_str().unwrap().to_string();
        let arm = entry["arm"].as_str().unwrap().to_string();
        let cfg_path = PathBuf::from(entry["path"].as_str().unwrap());
        let cfg = tiny_agent::Config::load(&cfg_path)
            .expect("config loads (runner guarantees a valid one)");
        let client = new_client(0);

        // A 臂脚本一次真实 run_code（guest registry 真值）+ 一个收尾文本轮；
        // B 臂只需一轮文本即可捕到请求侧工具表。
        let scripted = if arm == "A" {
            vec![
                resp_tool(
                    "inv-1",
                    "run_code",
                    json!({"code": "return tools.list()"}),
                    mk_usage(1, 1),
                ),
                resp_text("inventory done", mk_usage(1, 1)),
            ]
        } else {
            vec![resp_text("inventory done", mk_usage(1, 1))]
        };
        let (provider, state) = MockProvider::new(scripted);
        let mut agent = tiny_agent::build_agent_with_provider(
            &cfg,
            Box::new(provider),
            Vec::new(),
            &client,
            Vec::new(),
            "",
        )
        .expect("assemble agent from eval config");

        let system_prompt = agent.system_prompt().map(str::to_string);
        let tool_defs: Vec<_> = agent
            .tool_defs()
            .iter()
            .map(|d| {
                json!({
                    "name": d.name,
                    "description": d.description,
                    "schema": d.schema,
                })
            })
            .collect();
        let agent_tool_names = agent.tool_names();

        let outcome = agent.run(&mut |_| {}).expect("scripted run");
        let captured = state.captured();
        let request_tool_names: Vec<Vec<String>> =
            captured.iter().map(|c| c.tool_names.clone()).collect();

        // guest 可调用工具：从真实 run_code 调用的结果里解析（A 臂）。
        let mut guest_tools: Option<serde_json::Value> = None;
        if arm == "A" {
            let rec = outcome
                .tool_calls
                .iter()
                .find(|c| c.name == "run_code")
                .expect("A arm scripted a run_code call");
            assert!(!rec.is_error, "run_code failed: {}", rec.output);
            let parsed: serde_json::Value = serde_json::from_str(&rec.output)
                .unwrap_or_else(|_| panic!("run_code output not JSON: {}", rec.output));
            guest_tools = Some(parsed);
        }

        out.insert(
            key,
            json!({
                "arm": arm,
                "task": entry["task"],
                "config_path": cfg_path.display().to_string(),
                "config_tools_whitelist": cfg.tools,
                "code_enabled": cfg.code.enabled,
                "agent_tool_names": agent_tool_names,
                "request_tool_names": request_tool_names,
                "tool_defs": tool_defs,
                "system_prompt": system_prompt,
                "guest_tools": guest_tools,
                "run_stop_reason": outcome.stop_reason,
            }),
        );
    }

    let out_path = PathBuf::from(spec["out"].as_str().unwrap());
    std::fs::write(
        &out_path,
        serde_json::to_string_pretty(&serde_json::Value::Object(out)).unwrap(),
    )
    .expect("write inventory json");
}
