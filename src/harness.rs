use crate::agent::{Agent, AgentEvent, RunOutcome};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;

/// Run the agent to completion in headless mode:
/// - `json = false`: stream human-readable events to stdout/stderr.
/// - `json = true`:  stream one JSON object per event (NDJSON) to stdout.
///
/// Then write a structured result file (always — even on failure). Returns the
/// process exit code. On a transient error (`agent.run` returns Err — provider /
/// network / timeout), retry up to `retries` more times with exponential
/// backoff (shared with batch); deterministic outcomes (end_turn, max_turns,
/// cancelled, timeout) come back as Ok and are never retried.
pub fn run(
    agent: &mut Agent,
    output_path: Option<&Path>,
    json: bool,
    retries: usize,
) -> Result<i32> {
    run_with_retries(agent, output_path, json, retries, std::thread::sleep)
}

/// `sleep` is injected so unit tests run the retry loop without waiting on real
/// backoff (the live path passes `std::thread::sleep`). The same `&mut Agent` is
/// reused across attempts — a provider Err happens before any message is
/// appended (the `?` is on `provider.chat`), and `agent.run` resets the cancel
/// flag at entry, so a retry starts clean. Caveat: if compression fired on a
/// failed attempt it already folded the history (and saved the session), so the
/// retry runs on the shorter history — safe and idempotent (it won't re-compress
/// or double-bill), just not a perfect in-place restart. Token totals reflect
/// only the final attempt (matching batch); `attempts` in result.json records
/// how many ran.
pub(crate) fn run_with_retries(
    agent: &mut Agent,
    output_path: Option<&Path>,
    json: bool,
    retries: usize,
    mut sleep: impl FnMut(std::time::Duration),
) -> Result<i32> {
    let mut at_line_start = true;
    // 1-based count of completed tries → result.json's `attempts`. We retry
    // while `attempt <= retries`, i.e. at most retries+1 total attempts.
    let mut attempt = 1usize;
    let (code, outcome, error) = loop {
        let result: std::result::Result<RunOutcome, anyhow::Error> =
            agent.run(&mut |ev| {
                if json {
                    emit_json(&ev);
                    return;
                }
                match ev {
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
                    AgentEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cache_creation_input_tokens,
                        cache_read_input_tokens,
                    } => {
                        let cache = if cache_creation_input_tokens > 0 || cache_read_input_tokens > 0 {
                            format!(
                                " · cache {}write {}read",
                                cache_creation_input_tokens, cache_read_input_tokens
                            )
                        } else {
                            String::new()
                        };
                        eprintln!("[tokens: {input_tokens}↑ {output_tokens}↓{cache}]");
                    }
                    AgentEvent::SubAgent { name, event } => {
                        // Render the child's event prefixed with its identity, so
                        // a human can follow each sub-agent's progress —
                        // especially when several run in parallel and interleave.
                        // Usage is already folded into the run totals at the agent
                        // level, so it's not re-shown here.
                        match &*event {
                            AgentEvent::TextDelta(t) => {
                                print!("[{name}] {t}");
                                let _ = std::io::stdout().flush();
                                at_line_start = t.ends_with('\n');
                            }
                            AgentEvent::ToolCall { name: tn, input } => {
                                println!("\n[{name}] [tool →] {tn} {}", compact(input));
                            }
                            AgentEvent::ToolResult {
                                name: tn,
                                is_error,
                                content,
                            } => {
                                let tag = if *is_error { "[tool ✗]" } else { "[tool ✓]" };
                                println!("[{name}] {tag} {tn}: {}", crate::agent::preview(content));
                            }
                            AgentEvent::Done { .. } => {} // implicit in the result
                            _ => {} // TurnStart/Compressed/Usage/SubAgent: noisy or folded
                        }
                    }
                }
            });
        match result {
            Ok(o) => break (exit_code_for(&o.stop_reason), Some(o), None),
            Err(e) => {
                if attempt <= retries {
                    let backoff =
                        std::time::Duration::from_secs(crate::retry::backoff_secs(attempt - 1));
                    let msg = format!("{e:#}");
                    if json {
                        // Surface the retry to NDJSON consumers between attempts
                        // so they can follow progress, not just see a gap.
                        let v = json!({
                            "type": "retry",
                            "attempt": attempt,
                            "max": retries,
                            "backoff_secs": backoff.as_secs(),
                            "error": msg,
                        });
                        println!(
                            "{}",
                            serde_json::to_string(&v).unwrap_or_else(|_| "{}".into())
                        );
                        let _ = std::io::stdout().flush();
                    } else {
                        eprintln!("[transient] {msg}; retry {attempt}/{retries} in {backoff:?}");
                    }
                    sleep(backoff);
                    // A Ctrl-C during the backoff flips the cancel flag, but
                    // the next agent.run clears it at entry and would proceed
                    // — making the first Ctrl-C look like a no-op (only the
                    // second SIGINT force-exits). Honor it here instead.
                    if agent.cancelled() {
                        break (
                            EXIT_CANCELLED,
                            None,
                            Some("cancelled during retry backoff".into()),
                        );
                    }
                    attempt += 1;
                    continue;
                }
                break (EXIT_ERROR, None, Some(format!("{e:#}")));
            }
        }
    };
    // Always produce a result artifact, even on failure: the previous `?` here
    // skipped write_result on any error, leaving CI with nothing but an opaque
    // non-zero exit. Batch (run_one) already did this; single-run now matches.
    if let Some(path) = output_path {
        write_result(path, outcome.as_ref(), error.as_deref(), attempt)?;
        if !json {
            eprintln!("result written to {}", path.display());
        }
    }
    Ok(code)
}

/// Exit codes. 0 = success; 124 = timed out (GNU `timeout` convention); 130 =
/// cancelled (128 + SIGINT, the conventional "interrupted" code); 1 = anything
/// else (max_turns, provider error, …). Scripts can branch on `$?`; the result
/// JSON carries the precise stop_reason / error for detail.
const EXIT_ERROR: i32 = 1;
const EXIT_TIMEOUT: i32 = 124;
const EXIT_CANCELLED: i32 = 130;

fn exit_code_for(stop_reason: &str) -> i32 {
    match stop_reason {
        "end_turn" => 0,
        "timeout" => EXIT_TIMEOUT,
        "cancelled" => EXIT_CANCELLED,
        _ => EXIT_ERROR,
    }
}

/// Print one JSON object per event (NDJSON) for `--json` mode, so a pipeline
/// can consume progress live instead of waiting for the final result file.
/// JSON shape for one event. Factored out so `SubAgent` can nest its child's
/// event as `event_json(child)` without re-listing every variant's fields.
fn event_json(ev: &AgentEvent) -> Value {
    // ev is borrowed, so the match bindings are references — clone the owned
    // fields (String/Value) and deref the Copy ones for serde_json::Value.
    match ev {
        AgentEvent::TurnStart(n) => json!({"type": "turn", "n": *n}),
        AgentEvent::TextDelta(t) => json!({"type": "text", "delta": t.clone()}),
        AgentEvent::ToolCall { name, input } => {
            json!({"type": "tool_call", "name": name.clone(), "input": input.clone()})
        }
        AgentEvent::ToolResult {
            name,
            is_error,
            content,
        } => {
            json!({
                "type": "tool_result",
                "name": name.clone(),
                "is_error": *is_error,
                "content": content.clone(),
            })
        }
        AgentEvent::Usage {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
        } => {
            json!({
                "type": "usage",
                "input_tokens": *input_tokens,
                "output_tokens": *output_tokens,
                "cache_creation_input_tokens": *cache_creation_input_tokens,
                "cache_read_input_tokens": *cache_read_input_tokens,
            })
        }
        AgentEvent::Compressed { dropped, kept } => {
            json!({"type": "compressed", "dropped": *dropped, "kept": *kept})
        }
        AgentEvent::SubAgent { name, event } => {
            json!({
                "type": "subagent",
                "name": name.clone(),
                "event": event_json(event),
            })
        }
        AgentEvent::Done { stop_reason } => {
            json!({"type": "done", "stop_reason": stop_reason.clone()})
        }
    }
}

fn emit_json(ev: &AgentEvent) {
    let v = event_json(ev);
    println!(
        "{}",
        serde_json::to_string(&v).unwrap_or_else(|_| "{}".into())
    );
    let _ = std::io::stdout().flush();
}

pub fn write_result(
    path: &Path,
    outcome: Option<&RunOutcome>,
    error: Option<&str>,
    attempts: usize,
) -> Result<()> {
    // json! takes values by ownership, so the &RunOutcome fields are cloned
    // here; Option<&str> is turned into a JSON null/string explicitly (Value
    // has no From<Option<_>>).
    let error_val: serde_json::Value = match error {
        Some(s) => json!(s),
        None => serde_json::Value::Null,
    };
    let result = match outcome {
        Some(o) => {
            let tool_calls: Vec<_> = o
                .tool_calls
                .iter()
                .map(|c| {
                    json!({
                        "name": c.name.clone(),
                        "input": c.input.clone(),
                        "output": c.output.clone(),
                        "is_error": c.is_error,
                    })
                })
                .collect();
            json!({
                "success": o.stop_reason == "end_turn",
                "stop_reason": o.stop_reason.clone(),
                "turns": o.turns,
                "attempts": attempts,
                "elapsed_secs": o.elapsed.as_secs_f64(),
                "input_tokens": o.input_tokens,
                "output_tokens": o.output_tokens,
                "cache_creation_input_tokens": o.cache_creation_input_tokens,
                "cache_read_input_tokens": o.cache_read_input_tokens,
                "summary": o.final_text.clone(),
                "tool_calls": tool_calls,
                "error": error_val,
            })
        }
        None => json!({
            "success": false,
            "stop_reason": "error",
            "attempts": attempts,
            "error": error_val,
        }),
    };
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
    use crate::agent::{RunOutcome, ToolCallRecord};
    use serde_json::json;

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

    #[test]
    fn write_result_carries_output_tokens_and_error() {
        let dir = std::env::temp_dir().join(format!("ta-res-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let o = RunOutcome {
            final_text: "done".into(),
            turns: 2,
            stop_reason: "end_turn".into(),
            tool_calls: vec![ToolCallRecord {
                name: "read_file".into(),
                input: json!({"path": "a.txt"}),
                output: "hello world".into(),
                is_error: false,
            }],
            input_tokens: 100,
            output_tokens: 5,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            elapsed: std::time::Duration::from_secs_f64(1.5),
        };
        // Success path: tool output + token totals + summary all present.
        let path = dir.join("out.json");
        write_result(&path, Some(&o), None, 1).unwrap();
        let s = std::fs::read_to_string(&path).unwrap();
        assert!(s.contains("\"output\"") && s.contains("hello world"), "tool output missing");
        assert!(
            s.contains("\"input_tokens\"") && s.contains("\"output_tokens\""),
            "token totals missing"
        );
        assert!(s.contains("\"summary\""));

        // Failure path: agent.run returned Err — still write a result artifact
        // (this is the gap the old `?`-skips-write_result had).
        let epath = dir.join("err.json");
        write_result(&epath, None, Some("provider 503"), 1).unwrap();
        let es = std::fs::read_to_string(&epath).unwrap();
        assert!(es.contains("\"success\": false"));
        assert!(es.contains("provider 503"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exit_code_distinguishes_outcomes() {
        assert_eq!(exit_code_for("end_turn"), 0);
        assert_eq!(exit_code_for("timeout"), 124);
        assert_eq!(exit_code_for("cancelled"), 130);
        assert_eq!(exit_code_for("max_turns_reached"), 1);
        assert_eq!(exit_code_for("max_tokens"), 1);
    }

    #[test]
    fn event_json_nests_subagent_event_with_name() {
        // NDJSON consumers see each forwarded child event as
        // {"type":"subagent","name":..,"event":<child>}, with the child's own
        // shape recursed via event_json — so identity + the inner event both
        // survive the serialization (parallel sub-agents interleave by `name`).
        let ev = AgentEvent::SubAgent {
            name: "scout".into(),
            event: Box::new(AgentEvent::TextDelta("hi".into())),
        };
        let v = event_json(&ev);
        assert_eq!(v["type"], "subagent");
        assert_eq!(v["name"], "scout");
        assert_eq!(v["event"]["type"], "text");
        assert_eq!(v["event"]["delta"], "hi");
    }
}
