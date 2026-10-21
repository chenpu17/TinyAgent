//! Executes the shipped batch recipe, including its failure paths, in real QuickJS.
#![cfg(feature = "code")]

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use tiny_agent::config::CodeConfig;
use tiny_agent::tools::RunCodeTool;
use tiny_agent::{CallbackTool, Config, Tool, ToolCtx, ToolDef, ToolOutput};

#[derive(Clone, Copy)]
enum Scenario {
    Complete,
    MissingPageField,
    RepeatedCursor,
    BusinessFailure,
    IneffectiveWrite,
    MissingVerificationField,
    InsufficientBudget,
}

struct State {
    items: BTreeMap<String, Value>,
    deletes: Vec<String>,
    plan: Option<Value>,
}

fn execute(scenario: Scenario) -> (ToolOutput, State) {
    let items = [
        ("a", true, false),
        ("b", true, true),
        ("c", false, false),
        ("d", false, true),
        ("e", true, false),
    ]
    .into_iter()
    .map(|(id, archived, liked)| {
        (
            id.to_string(),
            json!({"id": id, "archived": archived, "liked": liked}),
        )
    })
    .collect();
    let state = Arc::new(Mutex::new(State {
        items,
        deletes: vec![],
        plan: None,
    }));
    let extras = ["mcp_list_items", "mcp_delete", "mcp_get_item"]
        .into_iter()
        .map(|name| {
            let state = state.clone();
            let callback = CallbackTool::new(
                ToolDef {
                    name: name.into(),
                    description: "Fictional API for the documented batch recipe".into(),
                    schema: json!({"type": "object"}),
                },
                move |input: &Value, _: &ToolCtx| {
                    let mut state = state.lock().unwrap();
                    let response = match name {
                        "mcp_list_items" => {
                            assert_eq!(input["limit"], 100);
                            if input["cursor"].is_null() {
                                json!({"items": [state.items["a"], state.items["b"]], "next_cursor": "p2"})
                            } else {
                                assert_eq!(input["cursor"], "p2");
                                match scenario {
                                    Scenario::MissingPageField => json!({"items": []}),
                                    Scenario::RepeatedCursor => json!({"items": [], "next_cursor": "p2"}),
                                    _ => json!({"items": [state.items["a"], state.items["c"], state.items["d"], state.items["e"]], "next_cursor": null}),
                                }
                            }
                        }
                        "mcp_delete" => {
                            let id = input["id"].as_str().unwrap();
                            state.deletes.push(id.into());
                            if id == "e" && matches!(scenario, Scenario::BusinessFailure) {
                                json!({"ok": false})
                            } else {
                                if !(id == "e" && matches!(scenario, Scenario::IneffectiveWrite)) {
                                    state.items.remove(id);
                                }
                                json!({"ok": true})
                            }
                        }
                        "mcp_get_item" => {
                            let id = input["id"].as_str().unwrap();
                            if id == "e" && matches!(scenario, Scenario::MissingVerificationField) {
                                json!({})
                            } else {
                                json!({"item": state.items.get(id)})
                            }
                        }
                        _ => unreachable!(),
                    };
                    Ok(ToolOutput::text(response.to_string()))
                },
            )
            .with_code_access();
            Arc::new(Box::new(callback) as Box<dyn Tool>)
        })
        .collect();
    let cfg: Config = serde_json::from_value(json!({
        "provider": "openai", "api_key_env": "NONE", "model": "offline",
        "tools": ["read_file", "write_file"], "inject_environment": false
    }))
    .unwrap();
    let tool = RunCodeTool::new(
        &cfg,
        Arc::new(RwLock::new(CodeConfig {
            max_tool_calls: if matches!(scenario, Scenario::InsufficientBudget) {
                12
            } else {
                CodeConfig::default().max_tool_calls
            },
            ..CodeConfig::default()
        })),
        &reqwest::blocking::Client::new(),
        "",
        vec![],
        extras,
    );
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "tinyagent-skill-batch-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = ToolCtx {
        tool_absent_notes: Vec::new(),
        workdir: dir.clone(),
        max_output_chars: 5000,
        restrict_to_workdir: true,
        cancel: None,
        shell_timeout_secs: 5,
        hooks: None,
        subagent: None,
        budget: None,
        deadline: None,
    };
    // Windows checkouts may carry CRLF (core.autocrlf); anchor the split on
    // LF so recipe extraction works regardless of the file's EOL on disk.
    let skill = include_str!("../src/tools/run_code_skill.md").replace("\r\n", "\n");
    let code = skill
        .split("```js\n// batch-audit-example:")
        .nth(1)
        .unwrap();
    let code = code
        .split_once('\n')
        .unwrap()
        .1
        .split("```")
        .next()
        .unwrap();
    let out = tool
        .run_with_emit(&json!({"code": code}), &ctx, &mut |_| {})
        .unwrap();
    state.lock().unwrap().plan = std::fs::read_to_string(dir.join("batch-plan.json"))
        .ok()
        .map(|text| serde_json::from_str(&text).unwrap());
    std::fs::remove_dir_all(dir).unwrap();
    drop(tool);
    let state = Arc::try_unwrap(state).ok().unwrap().into_inner().unwrap();
    (out, state)
}

#[test]
fn batch_recipe_rejects_incomplete_pagination_before_any_mutation() {
    let (out, state) = execute(Scenario::MissingPageField);
    assert!(
        out.is_error && out.content.contains("invalid page response"),
        "{}",
        out.content
    );
    assert!(state.deletes.is_empty());
    assert_eq!(state.items.len(), 5);
}

#[test]
fn batch_recipe_rejects_repeated_cursor_before_any_mutation() {
    let (out, state) = execute(Scenario::RepeatedCursor);
    assert!(
        out.is_error && out.content.contains("pagination did not advance"),
        "{}",
        out.content
    );
    assert!(state.deletes.is_empty());
}

#[test]
fn batch_recipe_deduplicates_targets_and_preserves_predicate_boundaries() {
    let (out, state) = execute(Scenario::Complete);
    assert!(!out.is_error, "{}", out.content);
    let summary: Value = serde_json::from_str(&out.content).unwrap();
    assert_eq!(summary["complete"], true);
    assert_eq!(summary["verified"], true);
    assert_eq!(summary["targets"], 2);
    assert_eq!(summary["succeeded"], 2);
    assert_eq!(summary["kept"], 3);
    assert_eq!(state.deletes, ["a", "e"]);
    assert_eq!(
        state.items.keys().map(String::as_str).collect::<Vec<_>>(),
        ["b", "c", "d"]
    );
}

#[test]
fn batch_recipe_reports_business_failure_as_incomplete() {
    let (out, state) = execute(Scenario::BusinessFailure);
    assert!(!out.is_error, "{}", out.content);
    let summary: Value = serde_json::from_str(&out.content).unwrap();
    assert_eq!(summary["complete"], false);
    assert_eq!(summary["succeeded"], 1);
    assert_eq!(summary["failed"], 1);
    assert_eq!(summary["failed_sample"], json!(["e"]));
    assert!(state.items.contains_key("e"));
}

#[test]
fn batch_recipe_detects_success_response_without_effect() {
    let (out, _) = execute(Scenario::IneffectiveWrite);
    assert!(
        out.is_error && out.content.contains("final state mismatch: e"),
        "{}",
        out.content
    );
}

#[test]
fn batch_recipe_rejects_missing_item_field_as_proof_of_deletion() {
    let (out, _) = execute(Scenario::MissingVerificationField);
    assert!(
        out.is_error && out.content.contains("invalid verification response: e"),
        "{}",
        out.content
    );
}

#[test]
fn batch_recipe_preserves_plan_when_budget_cannot_cover_execution_and_verification() {
    let (out, state) = execute(Scenario::InsufficientBudget);
    assert!(
        out.is_error && out.content.contains("plan saved: batch-plan.json"),
        "{}",
        out.content
    );
    assert!(state.deletes.is_empty());
    assert_eq!(state.items.len(), 5);
    assert_eq!(
        state.plan.unwrap(),
        json!({"keep": ["b", "c", "d"], "targets": ["a", "e"]})
    );
}

#[test]
fn response_recipe_unwraps_json_without_treating_invalid_results_as_empty_pages() {
    let cfg: Config = serde_json::from_value(json!({
        "provider": "openai", "api_key_env": "NONE", "model": "offline",
        "tools": [], "inject_environment": false
    }))
    .unwrap();
    let callback = CallbackTool::new(
        ToolDef {
            name: "fixture_page".into(),
            description: "Return a JSON page exactly as an MCP text result would".into(),
            schema: json!({"type": "object"}),
        },
        |input: &Value, _: &ToolCtx| Ok(ToolOutput::text(input["page"].to_string())),
    )
    .with_code_access();
    let tool = RunCodeTool::new(
        &cfg,
        Arc::new(RwLock::new(CodeConfig::default())),
        &reqwest::blocking::Client::new(),
        "",
        vec![],
        vec![Arc::new(Box::new(callback) as Box<dyn Tool>)],
    );
    let skill = include_str!("../src/tools/run_code_skill.md").replace("\r\n", "\n");
    let recipe = skill
        .split("```js\n// response-array-example:")
        .nth(1)
        .unwrap()
        .split_once('\n')
        .unwrap()
        .1
        .split("```")
        .next()
        .unwrap();
    let code = format!(
        r#"{recipe}
        const pages = [[{{id: "a"}}], {{response: [{{id: "b"}}]}},
                       {{response: []}}, {{}}, {{response: null}},
                       {{response: {{error: "denied"}}}}, {{items: []}}];
        return pages.map(page => {{
            try {{ return responseRows(tools.call("fixture_page", {{page}})); }}
            catch (e) {{ return {{invalid: true}}; }}
        }});"#
    );
    let ctx = ToolCtx {
        tool_absent_notes: Vec::new(),
        workdir: std::env::temp_dir(),
        max_output_chars: 5000,
        restrict_to_workdir: true,
        cancel: None,
        shell_timeout_secs: 5,
        hooks: None,
        subagent: None,
        budget: None,
        deadline: None,
    };
    let out = tool
        .run_with_emit(&json!({"code": code}), &ctx, &mut |_| {})
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        serde_json::from_str::<Value>(&out.content).unwrap(),
        json!([[{"id": "a"}], [{"id": "b"}], [], {"invalid": true},
               {"invalid": true}, {"invalid": true}, {"invalid": true}])
    );
    assert_eq!(out.meta.unwrap()["call_count"], 7);
}
