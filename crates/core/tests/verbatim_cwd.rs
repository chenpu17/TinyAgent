//! Assemble's workdir fallback must apply the same normalization as the
//! configured path. A config WITH `workdir` is canonicalized + verbatim-stripped
//! by `Config::validate_and_normalize` (via `resolve_workdir`); a config with
//! NO workdir falls back to `std::env::current_dir()` — which on Windows can
//! itself be the verbatim `\\?\` spelling (e.g. when the process was launched
//! from one), and the raw form must not reach the model-visible "Working
//! directory:" env line (cmd.exe rejects `cd \\?\...` spellings).
//!
//! The regression sets the PROCESS cwd — a mutation every test in the same
//! binary would see — so it lives in its own integration-test binary and can
//! never race a parallel test.
#![cfg(feature = "mock")]

#[cfg(windows)]
#[test]
fn workdir_fallback_strips_verbatim_prefix_from_env_header() {
    use serde_json::json;
    use tiny_agent::provider::mock::MockProvider;

    let plain = std::env::current_dir().expect("cwd readable");
    let verbatim = std::fs::canonicalize(&plain).expect("canonicalize");
    if let Err(e) = std::env::set_current_dir(&verbatim) {
        eprintln!("set_current_dir(verbatim) refused on this host: {e} — skipping");
        return;
    }
    // Build in the verbatim cwd with NO configured workdir (the fallback
    // branch) and env injection ON, so the env header carries the workdir.
    let cfg: tiny_agent::Config = serde_json::from_value(json!({
        "provider": "openai",
        "api_key_env": "NONE",
        "model": "m",
        "tools": [],
        "inject_environment": true
    }))
    .expect("config");
    let client = tiny_agent::provider::new_client(0);
    let (provider, _s) = MockProvider::new(vec![]);
    let built = tiny_agent::build_agent_with_provider(
        &cfg,
        Box::new(provider),
        Vec::new(),
        &client,
        vec![],
        "",
    );
    // Restore FIRST: a process-wide mutation must not leak into other tests,
    // even when the assertions below panic.
    std::env::set_current_dir(&plain).expect("restore cwd");
    let agent = built.expect("assemble in verbatim cwd");
    let sp = agent
        .system_prompt()
        .expect("env header present")
        .to_string();
    let wd = sp
        .lines()
        .find(|l| l.starts_with("- Working directory:"))
        .expect("Working directory line");
    assert!(
        !wd.contains(r"\\?\"),
        "the un-normalized current_dir() fallback leaked the verbatim \
         prefix into the model-visible env header: {wd}"
    );
}
