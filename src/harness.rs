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
            preview,
        } => {
            let tag = if is_error { "[tool ✗]" } else { "[tool ✓]" };
            println!("{tag} {name}: {preview}");
        }
        AgentEvent::Done { stop_reason } => {
            println!("\n[done: {stop_reason}]");
        }
        AgentEvent::Compressed { dropped, kept } => {
            println!("\n[context compressed: dropped {dropped} messages, {kept} kept]");
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
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, serde_json::to_string_pretty(&result)?)
        .with_context(|| format!("cannot write result file: {}", path.display()))
}

fn compact(v: &serde_json::Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 200 {
        format!("{}…", s.chars().take(200).collect::<String>())
    } else {
        s
    }
}
