//! Shared helpers for run_code's per-topic test modules (scratch workdir,
//! tool builders, ToolCtx, the one-shot runner).

use super::*;

/// A scratch workdir per TEST (guest file tools resolve against the ctx
/// workdir; the repo cwd must not be touched). Unique per call: tests in
/// one binary share a pid, and a shared dir let one test's `out/` entries
/// collide with another's symlink setup.
pub(super) fn scratch() -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "ta-run-code-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub(super) fn tool_with(code_cfg: CodeConfig) -> RunCodeTool {
    tool_with_tools(code_cfg, r#"["read_file","list_dir"]"#)
}

/// [`tool_with`] with an explicit whitelist (tests that need write_file
/// or shell in the guest registry).
pub(super) fn tool_with_tools(code_cfg: CodeConfig, tools: &str) -> RunCodeTool {
    let cfg: Config = serde_json::from_str(&format!(
        r#"{{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":{tools}}}"#
    ))
    .unwrap();
    RunCodeTool::new(
        &cfg,
        Arc::new(RwLock::new(code_cfg)),
        &reqwest::blocking::Client::new(),
        "k",
        vec![],
        vec![],
    )
}

pub(super) fn ctx(workdir: &std::path::Path) -> ToolCtx {
    ToolCtx {
        tool_absent_notes: Vec::new(),
        workdir: workdir.to_path_buf(),
        max_output_chars: 1000,
        restrict_to_workdir: false,
        cancel: None,
        shell_timeout_secs: 5,
        hooks: None,
        subagent: None,
        budget: None,
        deadline: None,
    }
}

pub(super) fn run_code(code: &str, code_cfg: CodeConfig) -> ToolOutput {
    tool_with(code_cfg)
        .run_with_emit(&json!({ "code": code }), &ctx(&scratch()), &mut |_| {})
        .unwrap()
}

/// after_tool observation sink for the batch deny-shape test (statics are
/// fine here: the test clears it on entry and nothing else pushes).
pub(super) static AFTER_SEEN: std::sync::Mutex<Vec<(String, bool)>> =
    std::sync::Mutex::new(Vec::new());

pub(super) fn include_cfg() -> CodeConfig {
    CodeConfig {
        include: true,
        ..CodeConfig::default()
    }
}

/// [`lib_tool`] with the batch channel on (tests that need callBatch AND
/// the lib/file registry in one guest).
pub(super) fn lib_tool_batch() -> RunCodeTool {
    tool_with_tools(
        CodeConfig {
            call_batch: true,
            ..include_cfg()
        },
        r#"["read_file","write_file","list_dir"]"#,
    )
}

pub(super) fn js_out(tool: &RunCodeTool, dir: &std::path::Path, code: &str) -> Value {
    let out = tool
        .run_with_emit(&json!({ "code": code }), &ctx(dir), &mut |_| {})
        .unwrap();
    assert!(!out.is_error, "guest failed: {}", out.content);
    serde_json::from_str(&out.content).unwrap_or(json!(null))
}

pub(super) fn llm_cfg() -> CodeConfig {
    CodeConfig {
        llm: crate::config::LlmConfig {
            enabled: true,
            default_model: Some("m1".into()),
            models: vec!["m1".into(), "m2".into()],
        },
        ..CodeConfig::default()
    }
}

/// [`tool_with`] + the LD-16 cell fill: a scripted mock provider rides
/// the shared cell exactly the way `Agent::set_code_tool` installs it.
pub(super) fn llm_tool(
    code_cfg: CodeConfig,
    prov: std::sync::Arc<dyn crate::provider::Provider>,
) -> RunCodeTool {
    let t = tool_with(code_cfg);
    let _ = t.provider_cell().set(prov);
    t
}

pub(super) fn gated_ctx(dir: &std::path::Path, cap: u64) -> ToolCtx {
    ToolCtx {
        budget: Some(crate::tools::BudgetGate::new(cap)),
        ..ctx(dir)
    }
}
