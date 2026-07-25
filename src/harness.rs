use crate::agent::{Agent, AgentEvent, RunOutcome};
use anyhow::{Context, Result};
use serde_json::json;
use std::io::Write;
use std::path::Path;

/// Run the agent to completion in headless mode:
/// stream events to stdout, then write a structured result file.
/// Returns the process exit code (0 = success, 1 = failure).
pub fn run(agent: &mut Agent, output_path: Option<&Path>) -> Result<i32> {
    let mut at_line_start = true;
    let outcome = agent.run(&mut |ev| match ev {
        AgentEvent::TurnStart(n) => {
            if !at_line_start {
                println!();
            }
            eprintln!("── turn {n} ──");
        }
        AgentEvent::TextDelta(t) => {
            print!("{t}");
            let _ = std::io::stdout().flush();
            at_line_start = t.ends_with('\n');
        }
        AgentEvent::ToolCall { name, input } => {
            println!("\n[tool →] {name} {}", compact(&input));
        }
        AgentEvent::ToolResult {
            name,
            is_error,
            content,
        } => {
            let tag = if is_error { "[tool ✗]" } else { "[tool ✓]" };
            println!("{tag} {name}: {}", crate::agent::preview(&content));
        }
        AgentEvent::Done { stop_reason } => {
            println!("\n[done: {stop_reason}]");
        }
        AgentEvent::Compressed { dropped, kept } => {
            println!("\n[context compressed: dropped {dropped} messages, {kept} kept]");
        }
        AgentEvent::Usage { input_tokens, output_tokens } => {
            eprintln!("[tokens: {input_tokens}↑ {output_tokens}↓]");
        }
    })?;

    if let Some(path) = output_path {
        write_result(path, &outcome)?;
        eprintln!("result written to {}", path.display());
    }

    let success = outcome.stop_reason == "end_turn";
    Ok(if success { 0 } else { 1 })
}

pub fn write_result(path: &Path, o: &RunOutcome) -> Result<()> {
    let tool_calls: Vec<_> = o
        .tool_calls
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "input": c.input,
                "is_error": c.is_error,
            })
        })
        .collect();
    let result = json!({
        "success": o.stop_reason == "end_turn",
        "stop_reason": o.stop_reason,
        "turns": o.turns,
        "elapsed_secs": o.elapsed.as_secs_f64(),
        "summary": o.final_text,
        "tool_calls": tool_calls,
    });
    let bytes = serde_json::to_string_pretty(&result)?;
    atomic_write(path, bytes.as_bytes())
}

/// Atomically write bytes to `path`: write a temp file in the same
/// directory, then rename. A crash mid-write never leaves a half-file —
/// result.json and report.json are read by CI, so they must not corrupt.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // Write + fsync the temp file before renaming, so a crash after rename
    // never exposes a zero-length file. (result.json / report.json are read
    // by CI and must not corrupt on power loss.)
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("cannot create {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("cannot sync {}", tmp.display()))?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp); // don't litter on failure
        return Err(e).with_context(|| format!("cannot save {}", path.display()));
    }
    Ok(())
}

fn compact(v: &serde_json::Value) -> String {
    crate::agent::preview(&v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_roundtrips_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!("ta-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.json");
        atomic_write(&path, b"{\"ok\":true}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"ok\":true}");
        // The temp file must be gone after the rename.
        assert!(!dir.join("out.tmp").exists());
        // Overwriting an existing file is also atomic.
        atomic_write(&path, b"v2").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v2");
        std::fs::remove_dir_all(&dir).ok();
    }
}
