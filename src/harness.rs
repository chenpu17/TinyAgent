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
    // Which sub-agent's text currently owns the stdout line (None = the parent
    // agent or no one). When a different sub-agent's TextDelta arrives mid-line,
    // we break to a new line and re-tag it — parallel sub-agents interleave, and
    // without this their streamed text would mash together unlabelled.
    let mut current_sub_name: Option<String> = None;
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
                        at_line_start = true; // we're on a fresh stdout line now
                        current_sub_name = None;
                    }
                    AgentEvent::TextDelta(t) => {
                        print!("{t}");
                        let _ = std::io::stdout().flush();
                        at_line_start = t.ends_with('\n');
                        current_sub_name = None; // parent agent owns this line now
                    }
                    AgentEvent::ToolCall { name, input } => {
                        println!("\n[tool →] {name} {}", compact(&input));
                        at_line_start = true; // println ended on a fresh line
                        current_sub_name = None;
                    }
                    AgentEvent::ToolResult {
                        name,
                        is_error,
                        content,
                    } => {
                        let tag = if is_error { "[tool ✗]" } else { "[tool ✓]" };
                        println!("{tag} {name}: {}", crate::agent::preview(&content));
                        at_line_start = true;
                        current_sub_name = None;
                    }
                    AgentEvent::Done { stop_reason } => {
                        println!("\n[done: {stop_reason}]");
                        at_line_start = true;
                        current_sub_name = None;
                    }
                    AgentEvent::Compressed {
                        dropped,
                        kept,
                        trigger,
                    } => {
                        let how = match trigger {
                            crate::agent::CompressTrigger::Preflight => "preflight",
                            crate::agent::CompressTrigger::Overflow => "overflow",
                        };
                        println!(
                            "\n[context compressed ({how}): dropped {dropped} messages, {kept} kept]"
                        );
                        at_line_start = true;
                        current_sub_name = None;
                    }
                    AgentEvent::Warning(w) => {
                        // Warnings go to stderr (stdout carries the transcript);
                        // break the line first if text is mid-stream.
                        if !at_line_start {
                            println!();
                            at_line_start = true;
                        }
                        eprintln!("[warn] {w}");
                        current_sub_name = None;
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
                                // Tag a sub-agent's text with [name]. Print the tag
                                // once at the start of ITS run of text — not on
                                // every streamed chunk (else fragments render as
                                // "[scout] Hel[scout] lo[scout] world"). And if a
                                // DIFFERENT sub-agent's text arrives mid-line
                                // (parallel interleaving), break to a new line and
                                // re-tag, so their streams don't mash together.
                                let same = current_sub_name.as_deref() == Some(name.as_str());
                                if at_line_start || !same {
                                    if !at_line_start {
                                        println!();
                                    }
                                    print!("[{name}] ");
                                    current_sub_name = Some(name.clone());
                                }
                                print!("{t}");
                                let _ = std::io::stdout().flush();
                                at_line_start = t.ends_with('\n');
                            }
                            AgentEvent::ToolCall { name: tn, input } => {
                                println!("\n[{name}] [tool →] {tn} {}", compact(input));
                                at_line_start = true;
                                current_sub_name = None;
                            }
                            AgentEvent::ToolResult {
                                name: tn,
                                is_error,
                                content,
                            } => {
                                let tag = if *is_error { "[tool ✗]" } else { "[tool ✓]" };
                                println!("[{name}] {tag} {tn}: {}", crate::agent::preview(content));
                                at_line_start = true;
                                current_sub_name = None;
                            }
                            AgentEvent::Done { .. } => {} // implicit in the result
                            AgentEvent::Warning(w) => {
                                if !at_line_start {
                                    println!();
                                    at_line_start = true;
                                }
                                eprintln!("[{name}] [warn] {w}");
                                current_sub_name = None;
                            }
                            _ => {} // TurnStart/Compressed/Usage/SubAgent: noisy or folded
                        }
                    }
                }
            });
        match result {
            Ok(o) => break (exit_code_for(&o.stop_reason), Some(o), None),
            Err(e) => {
                if attempt <= retries {
                    let backoff = crate::retry::backoff(attempt - 1);
                    let msg = format!("{e:#}");
                    if json {
                        // Surface the retry to NDJSON consumers between attempts
                        // so they can follow progress, not just see a gap.
                        let v = retry_event_json(attempt, retries, backoff, &msg);
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
    // json 模式下，重试耗尽 / 取消这两条路径 agent.run 没发 Done（Done 只在
    // 正常完成时由 agent 内部发出，已由上面的 emit_json 转发），补一个终态
    // 事件——否则 NDJSON 消费者无法区分"失败/取消"与"进程崩了"。
    if json {
        if let Some(err) = &error {
            let stop_reason = if code == EXIT_CANCELLED {
                "cancelled"
            } else {
                "error"
            };
            let v = done_event_json(stop_reason, err);
            println!(
                "{}",
                serde_json::to_string(&v).unwrap_or_else(|_| "{}".into())
            );
            let _ = std::io::stdout().flush();
        }
    }
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
        AgentEvent::Compressed {
            dropped,
            kept,
            trigger,
        } => {
            let how = match trigger {
                crate::agent::CompressTrigger::Preflight => "preflight",
                crate::agent::CompressTrigger::Overflow => "overflow",
            };
            // Additive field: existing consumers of dropped/kept are unaffected.
            json!({"type": "compressed", "dropped": *dropped, "kept": *kept, "trigger": how})
        }
        AgentEvent::Warning(w) => json!({"type": "warning", "message": w.clone()}),
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

/// NDJSON event emitted between attempts when a transient error is retried
/// (`--json` mode only). Lets a consumer follow retry progress instead of
/// seeing a gap. Factored out (like `event_json`) so its schema is unit-tested
/// — it's a consumer-facing contract, and a field-name typo would otherwise
/// go uncaught (the `json=true` path has no end-to-end test).
fn retry_event_json(
    attempt: usize,
    max: usize,
    backoff: std::time::Duration,
    error: &str,
) -> Value {
    json!({
        "type": "retry",
        "attempt": attempt,
        "max": max,
        // Jittered backoffs are sub-second; round to ms so consumers don't
        // see 0 for a 750ms wait.
        "backoff_secs": (backoff.as_secs_f64() * 1000.0).round() / 1000.0,
        "error": error,
    })
}

/// Synthetic terminal NDJSON event for the retry-exhausted / cancelled paths
/// (`--json` only): `agent.run` never emits a `Done` there (Done is emitted
/// only on a normal finish), so without this a consumer can't tell a
/// failure/cancel from a process that simply crashed. Factored out for the
/// same testability reason as `retry_event_json`.
fn done_event_json(stop_reason: &str, error: &str) -> Value {
    json!({
        "type": "done",
        "stop_reason": stop_reason,
        "error": error,
    })
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
    // fsync the parent directory so the rename's directory-entry update
    // survives a power loss. The file DATA is durable (sync_all above), but on
    // ext4 (when auto_da_alloc doesn't fire) and xfs the rename itself can be
    // lost without an explicit directory fsync — a crash could then expose the
    // pre-rename state (old content, or no file). APFS/NTFS handle directory
    // durability via their own transaction logs, so this is Unix-only and the
    // error is ignored: best-effort hardening, and the rename already succeeded.
    #[cfg(unix)]
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

fn compact(v: &serde_json::Value) -> String {
    crate::agent::preview(&v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{CompressTrigger, RunOutcome, ToolCallRecord};
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

    #[test]
    fn event_json_maps_each_variant_to_its_schema() {
        // Lock the NDJSON per-event schema: each variant maps to a distinct
        // `type` tag with its fields. A field-name typo here would silently
        // break every --json consumer, and before this only the SubAgent
        // variant had a test. (SubAgent stays covered by the test above.)
        // TurnStart → {"type":"turn","n":..}
        let v = event_json(&AgentEvent::TurnStart(3));
        assert_eq!(v["type"], "turn");
        assert_eq!(v["n"], 3);
        // TextDelta → {"type":"text","delta":..}
        let v = event_json(&AgentEvent::TextDelta("hi".into()));
        assert_eq!(v["type"], "text");
        assert_eq!(v["delta"], "hi");
        // ToolCall → {"type":"tool_call","name","input"}
        let v = event_json(&AgentEvent::ToolCall {
            name: "read_file".into(),
            input: json!({"path": "a.txt"}),
        });
        assert_eq!(v["type"], "tool_call");
        assert_eq!(v["name"], "read_file");
        assert_eq!(v["input"]["path"], "a.txt");
        // ToolResult → {"type":"tool_result","name","is_error","content"}
        let v = event_json(&AgentEvent::ToolResult {
            name: "read_file".into(),
            is_error: false,
            content: "hello".into(),
        });
        assert_eq!(v["type"], "tool_result");
        assert_eq!(v["name"], "read_file");
        assert_eq!(v["is_error"], false);
        assert_eq!(v["content"], "hello");
        // Usage → cache_* fields round-trip by exact name (a typo drops cache
        // cost from any downstream token accounting).
        let v = event_json(&AgentEvent::Usage {
            input_tokens: 100,
            output_tokens: 5,
            cache_creation_input_tokens: 200,
            cache_read_input_tokens: 300,
        });
        assert_eq!(v["type"], "usage");
        assert_eq!(v["input_tokens"], 100);
        assert_eq!(v["output_tokens"], 5);
        assert_eq!(v["cache_creation_input_tokens"], 200);
        assert_eq!(v["cache_read_input_tokens"], 300);
        // Compressed → trigger stringifies for both variants.
        let v = event_json(&AgentEvent::Compressed {
            dropped: 3,
            kept: 5,
            trigger: CompressTrigger::Preflight,
        });
        assert_eq!(v["type"], "compressed");
        assert_eq!(v["dropped"], 3);
        assert_eq!(v["kept"], 5);
        assert_eq!(v["trigger"], "preflight");
        let v = event_json(&AgentEvent::Compressed {
            dropped: 1,
            kept: 2,
            trigger: CompressTrigger::Overflow,
        });
        assert_eq!(v["trigger"], "overflow");
        // Warning → {"type":"warning","message":..}
        let v = event_json(&AgentEvent::Warning("careful".into()));
        assert_eq!(v["type"], "warning");
        assert_eq!(v["message"], "careful");
        // Done → {"type":"done","stop_reason":..}
        let v = event_json(&AgentEvent::Done {
            stop_reason: "end_turn".into(),
        });
        assert_eq!(v["type"], "done");
        assert_eq!(v["stop_reason"], "end_turn");
    }

    #[test]
    fn retry_event_json_schema_rounds_backoff_to_millis() {
        let v = retry_event_json(2, 3, std::time::Duration::from_millis(750), "boom");
        assert_eq!(v["type"], "retry");
        assert_eq!(v["attempt"], 2);
        assert_eq!(v["max"], 3);
        assert_eq!(v["error"], "boom");
        // 750ms must surface as 0.75, not 0 — the whole point of the ms
        // rounding (sub-second jittered backoffs would otherwise look instant).
        assert_eq!(v["backoff_secs"], 0.75);
    }

    #[test]
    fn done_event_json_carries_reason_and_error() {
        let v = done_event_json("error", "provider 503");
        assert_eq!(v["type"], "done");
        assert_eq!(v["stop_reason"], "error");
        assert_eq!(v["error"], "provider 503");
    }

    #[test]
    fn compact_previews_a_json_value() {
        // compact feeds a JSON value's string form through agent::preview; for
        // a short value nothing is truncated, so the key survives.
        let s = compact(&json!({"path": "a.txt"}));
        assert!(s.contains("a.txt"), "compact dropped the value: {s}");
    }

    #[test]
    fn atomic_write_creates_missing_parent_dirs() {
        let base =
            std::env::temp_dir().join(format!("ta-atomic-parent-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        // Neither `a/b` nor `a` exists — atomic_write must mkdir -p them (the
        // create_dir_all branch the basic test skips by pre-making the dir).
        let path = base.join("a/b/out.json");
        atomic_write(&path, b"ok").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ok");
        assert!(!base.join("a/b/out.tmp").exists(), "temp file left behind");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn write_result_records_nonzero_cache_tokens() {
        let dir = std::env::temp_dir().join(format!("ta-res-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let o = RunOutcome {
            final_text: "done".into(),
            turns: 1,
            stop_reason: "end_turn".into(),
            tool_calls: vec![],
            input_tokens: 1000,
            output_tokens: 50,
            cache_creation_input_tokens: 800,
            cache_read_input_tokens: 200,
            elapsed: std::time::Duration::from_secs(2),
        };
        let path = dir.join("cache.json");
        write_result(&path, Some(&o), None, 3).unwrap();
        // Parse back and assert by field name — a typo in the cache_* names
        // would silently drop cache cost from downstream accounting, and
        // substring checks couple to pretty-print spacing.
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["cache_creation_input_tokens"], 800);
        assert_eq!(parsed["cache_read_input_tokens"], 200);
        assert_eq!(parsed["attempts"], 3);
        std::fs::remove_dir_all(&dir).ok();
    }
}
