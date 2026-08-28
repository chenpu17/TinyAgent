use crate::agent::{extract_plan, Agent, AgentEvent, RunCost, RunOutcome};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;

/// Attribution metadata stamped into `result.json` (and, in part, the batch
/// report): which model / provider protocol produced the bill, and when the
/// run started. `finished_at` is taken inside `write_result`, so this struct
/// carries only the pre-run facts.
pub struct ResultMeta {
    pub model: String,
    pub provider: String,
    /// RFC 3339 UTC timestamp of the run's start, e.g. "2026-08-15T07:41:02Z".
    pub started_at: String,
}

impl ResultMeta {
    /// Build from the live agent — the single-run headless path derives it
    /// here so the 17 test call sites of `run_with_retries` need no changes.
    pub fn from_agent(agent: &Agent) -> Self {
        Self {
            model: agent.model().to_string(),
            provider: agent.provider().to_string(),
            started_at: now_rfc3339(),
        }
    }
}

/// Current wall clock as an RFC 3339 UTC timestamp (second precision).
pub(crate) fn now_rfc3339() -> String {
    rfc3339_utc(std::time::SystemTime::now())
}

/// Format a `SystemTime` as `YYYY-MM-DDTHH:MM:SSZ`. Hand-rolled (no chrono /
/// time / jiff dependency — house style: hand-written XDG lookup, hand-written
/// YAML frontmatter, kernel32 FFI): the classic days-from-civil inversion
/// (Hinnant) — exact for the full u64 second count, pure, and unit-testable
/// against known literals including leap days and year boundaries.
fn rfc3339_utc(t: std::time::SystemTime) -> String {
    let secs = match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        // Before the epoch (system clock skew): clamp to 0 rather than panic —
        // a timestamp is audit metadata, not worth failing a run over.
        Err(_) => 0,
    };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, rem / 60 % 60, rem % 60);
    // Civil-from-days (Howard Hinnant's algorithm): shifts the epoch to the
    // March-based civil calendar so leap days land at the end of the year,
    // making the month/day math branch-free.
    let z = days + 719_468; // days since 0000-03-01
    let era = z.div_euclid(146_097); // 400-year Gregorian cycle
    let doe = z.rem_euclid(146_097); // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // year of era
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year [0, 365]
    let mp = (5 * doy + 2) / 153; // month of the March-based year [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // day of month [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

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
/// or double-bill), just not a perfect in-place restart. Token totals and the
/// tool-call list cover EVERY attempt: a failed attempt's `RunError` carries the
/// tokens it burned and the tools it ran (their side effects are already in the
/// history/on disk), and we fold that into the final outcome so the reported
/// cost stays honest. `attempts` in result.json records how many ran.
pub(crate) fn run_with_retries(
    agent: &mut Agent,
    output_path: Option<&Path>,
    json: bool,
    retries: usize,
    mut sleep: impl FnMut(std::time::Duration),
) -> Result<i32> {
    // 1-based count of completed tries → result.json's `attempts`. We retry
    // while `attempt <= retries`, i.e. at most retries+1 total attempts.
    let mut attempt = 1usize;
    // Owns the two output sinks (stdout/stderr live, in-memory Vecs in tests)
    // and the cross-event line-tracking state. See HumanRenderer below.
    let mut renderer = HumanRenderer::new(std::io::stdout(), std::io::stderr());
    // Accumulated partial cost from failed attempts. A transient error
    // doesn't undo the tokens already streamed or the tools already run —
    // their side effects are in the history and on disk — so the final
    // result.json must fold these in for an honest total (P2-3).
    let mut acc = RunCost::default();
    // Stamp the run's start BEFORE the first attempt: retries must attribute
    // the whole run from its true beginning, not re-stamp at write time when
    // the backoff sleeps already consumed seconds-to-minutes.
    let started = now_rfc3339();
    // B4: set when the run died on an unfixable context overflow — the
    // terminal NDJSON event and result.json then say "context_overflow"
    // (matching batch's report vocabulary) instead of the generic "error".
    let mut failed_overflow = false;
    let (code, outcome, error) = loop {
        let result = agent.run(&mut |ev| {
            if json {
                emit_json(&ev);
                return;
            }
            renderer.render(&ev);
        });
        match result {
            Ok(mut o) => {
                // Bill every attempt: fold the failed attempts' cost into the
                // successful outcome before it's reported/written.
                acc.fold_into(&mut o);
                break (exit_code_for(&o.stop_reason), Some(o), None);
            }
            Err(re) => {
                // Record what this failed attempt burned before retrying.
                acc.add_failed(&re);
                // B4: an unfixable context overflow is deterministic — the
                // escape hatch already force-compressed (or couldn't) and a
                // retry re-sends an at-best equally-long request. Skip the
                // backoff ladder, which would only burn 1+2+4…s per attempt
                // to fail identically.
                if re.is_context_overflow() {
                    failed_overflow = true;
                    break (EXIT_ERROR, None, Some(format!("{re:#}")));
                }
                if attempt <= retries {
                    let backoff = crate::retry::backoff(attempt - 1);
                    let msg = format!("{re:#}");
                    if json {
                        // Surface the retry to NDJSON consumers between attempts
                        // so they can follow progress, not just see a gap.
                        let v = retry_event_json(attempt, retries, backoff, &msg);
                        print_ndjson(&serde_json::to_string(&v).unwrap_or_else(|_| "{}".into()));
                    } else {
                        eprintln!("[transient] {msg}; retry {attempt}/{retries} in {backoff:?}");
                    }
                    // Slice the backoff so a Ctrl-C during it is noticed within
                    // ~100ms instead of after the full ~40s sleep. The shared
                    // slice core (retry::sleep_cancelable_with) honors the
                    // injected `sleep` per slice (a no-op in tests) and reads
                    // the agent's own cancel flag. A Ctrl-C during the backoff
                    // flips that flag, but the next agent.run clears it at entry
                    // and would proceed — making the first Ctrl-C look like a
                    // no-op (only the second SIGINT force-exits) — so honor it
                    // here instead of falling through to the next attempt.
                    if crate::retry::sleep_cancelable_with(backoff, &mut sleep, || {
                        agent.cancelled()
                    }) {
                        break (
                            EXIT_CANCELLED,
                            None,
                            Some("cancelled during retry backoff".into()),
                        );
                    }
                    attempt += 1;
                    continue;
                }
                break (EXIT_ERROR, None, Some(format!("{re:#}")));
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
            } else if failed_overflow {
                "context_overflow"
            } else {
                "error"
            };
            let v = done_event_json(stop_reason, err);
            print_ndjson(&serde_json::to_string(&v).unwrap_or_else(|_| "{}".into()));
        }
    }
    // Always produce a result artifact, even on failure: the previous `?` here
    // skipped write_result on any error, leaving CI with nothing but an opaque
    // non-zero exit. Batch (run_one) already did this; single-run now matches.
    if let Some(path) = output_path {
        // When there's no outcome, record WHY: a cancel (exit 130) is
        // "cancelled", an unfixable context overflow is "context_overflow"
        // (matching batch's report vocabulary), anything else is "error".
        // Otherwise result.json would claim "error" for a user-initiated
        // cancel, disagreeing with the exit code and the NDJSON terminal
        // event (both already say "cancelled").
        let none_sr = if code == EXIT_CANCELLED {
            "cancelled"
        } else if failed_overflow {
            "context_overflow"
        } else {
            "error"
        };
        let mut meta = ResultMeta::from_agent(agent);
        meta.started_at = started;
        // A failed artifact write must not swallow the run's exit code: 130 /
        // 125 / 124 say what actually happened, and degrading them to 1 would
        // lie about a cancelled / timed-out run to CI. Warn-and-continue,
        // matching batch's write-failure policy.
        if let Err(e) = write_result(
            path,
            outcome.as_ref(),
            error.as_deref(),
            attempt,
            none_sr,
            Some(&acc),
            &meta,
        ) {
            eprintln!("[warn] failed to write {}: {e:#}", path.display());
        } else if !json {
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
const EXIT_BUDGET: i32 = 125;
const EXIT_CANCELLED: i32 = 130;

fn exit_code_for(stop_reason: &str) -> i32 {
    match stop_reason {
        "end_turn" => 0,
        "timeout" => EXIT_TIMEOUT,
        "budget_exceeded" => EXIT_BUDGET,
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
        AgentEvent::ThinkingDelta(t) => json!({"type": "thinking", "delta": t.clone()}),
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
        AgentEvent::TodoUpdate { todos } => {
            // Live plan-progress for `--json` pipelines. `todos` is the agent's
            // full current plan (full-replacement each `todo` call), serialized
            // as-is so a consumer can render its own checklist.
            json!({"type": "todo", "todos": todos})
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

/// Print one NDJSON line to stdout, IGNORING write errors: Rust installs
/// SIGPIPE=SIG_IGN, so a consumer closing the pipe (`--json … | head`) turns
/// `println!` into a panic (exit 101) mid-run. The human renderer already
/// treats I/O as best-effort for the same reason; the machine path must
/// match — a vanished consumer doesn't invalidate the run (result.json is
/// still written).
fn print_ndjson(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn emit_json(ev: &AgentEvent) {
    let v = event_json(ev);
    print_ndjson(&serde_json::to_string(&v).unwrap_or_else(|_| "{}".into()));
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
    // stop_reason recorded when `outcome` is None (run never produced an
    // outcome — e.g. cancelled during retry backoff, or a setup/provider
    // error). Ignored when outcome is Some (its own stop_reason is used).
    none_stop_reason: &str,
    // Accumulated partial cost from failed attempts, written ONLY when outcome
    // is None. A transient failure doesn't undo the tokens already streamed or
    // the tools already run — their side effects are in the history / on disk —
    // so the final result.json must fold them in for an honest total (P2). When
    // outcome is Some, its totals already include any folded-in cost, so this
    // is ignored. None ⇒ all-zero / empty (e.g. a setup error before any turn).
    none_acc: Option<&RunCost>,
    // Attribution: model / provider / started_at (finished_at is taken here).
    meta: &ResultMeta,
) -> Result<()> {
    // json! takes values by ownership, so the &RunOutcome fields are cloned
    // here; Option<&str> is turned into a JSON null/string explicitly (Value
    // has no From<Option<_>>).
    let error_val: serde_json::Value = match error {
        Some(s) => json!(s),
        None => serde_json::Value::Null,
    };
    // Attribution fields appear on EVERY result (not opt-in like `plan`/`meta`):
    // a bill you can't attribute to a model/provider/time isn't auditable.
    // Deliberate break from the byte-identical-to-before convention — pure
    // additive keys, consumers indexing by key are unaffected. `preserve_order`
    // is on, so insertion order below is the output order.
    let finished_at = now_rfc3339();
    let result = match outcome {
        Some(o) => {
            let tool_calls: Vec<_> = o
                .tool_calls
                .iter()
                .map(|c| {
                    let mut entry = json!({
                        "name": c.name.clone(),
                        "input": c.input.clone(),
                        "output": c.output.clone(),
                        "is_error": c.is_error,
                    });
                    // `meta` is sparse (only the `task` sub-agent sets it), so
                    // add the key only when present — leaf tools stay
                    // byte-identical to before the field existed.
                    if let Some(m) = &c.meta {
                        entry["meta"] = m.clone();
                    }
                    entry
                })
                .collect();
            let mut result = json!({
                "schema_version": 1,
                "success": o.stop_reason == "end_turn",
                "stop_reason": o.stop_reason.clone(),
                "model": meta.model.clone(),
                "provider": meta.provider.clone(),
                "turns": o.turns,
                "attempts": attempts,
                "started_at": meta.started_at.clone(),
                "finished_at": finished_at,
                "elapsed_secs": o.elapsed.as_secs_f64(),
                "input_tokens": o.input_tokens,
                "output_tokens": o.output_tokens,
                "cache_creation_input_tokens": o.cache_creation_input_tokens,
                "cache_read_input_tokens": o.cache_read_input_tokens,
                "summary": o.final_text.clone(),
                "tool_calls": tool_calls,
                "error": error_val,
            });
            // `plan` only when the agent actually used `todo`; omitted otherwise
            // so a result.json with no plan is byte-identical to pre-feature.
            if let Some(plan) = extract_plan(&o.tool_calls) {
                result["plan"] = json!(plan);
            }
            result
        }
        None => {
            // No final outcome, but the failed attempts still burned tokens and
            // ran tools. Fold the accumulated cost in so result.json reports an
            // honest total instead of silently zeroing everything (P2).
            let acc = none_acc.cloned().unwrap_or_default();
            let tool_calls: Vec<_> = acc
                .tool_calls
                .iter()
                .map(|c| {
                    let mut entry = json!({
                        "name": c.name.clone(),
                        "input": c.input.clone(),
                        "output": c.output.clone(),
                        "is_error": c.is_error,
                    });
                    if let Some(m) = &c.meta {
                        entry["meta"] = m.clone();
                    }
                    entry
                })
                .collect();
            let mut result = json!({
                "schema_version": 1,
                "success": false,
                "stop_reason": none_stop_reason,
                "model": meta.model.clone(),
                "provider": meta.provider.clone(),
                "turns": acc.turns,
                "attempts": attempts,
                "started_at": meta.started_at.clone(),
                "finished_at": finished_at,
                "elapsed_secs": acc.elapsed.as_secs_f64(),
                "input_tokens": acc.usage.input_tokens,
                "output_tokens": acc.usage.output_tokens,
                "cache_creation_input_tokens": acc.usage.cache_creation_input_tokens,
                "cache_read_input_tokens": acc.usage.cache_read_input_tokens,
                "summary": "",
                "tool_calls": tool_calls,
                "error": error_val,
            });
            // Even on failure, a partial plan (the agent's progress before it
            // died) is valuable audit info — derive it the same way.
            if let Some(plan) = extract_plan(&acc.tool_calls) {
                result["plan"] = json!(plan);
            }
            result
        }
    };
    let bytes = serde_json::to_string_pretty(&result)?;
    atomic_write(path, bytes.as_bytes())
}

/// Monotonic per-process suffix for atomic_write temp names. Module-level
/// (not inside the fn) so tests can predict the next allocation.
static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `<target>.<pid>.<n>.tmp` sibling of `path` — same directory, so the final
/// rename stays on one filesystem.
fn tmp_name(path: &Path, n: u64) -> std::path::PathBuf {
    match path.file_name() {
        Some(name) => {
            let mut base = name.to_os_string();
            base.push(format!(".{}.{}.tmp", std::process::id(), n));
            path.with_file_name(base)
        }
        None => path.with_extension("tmp"),
    }
}

/// Atomically write bytes to `path`: write a temp file in the same
/// directory, then rename. A crash mid-write never leaves a half-file —
/// result.json and report.json are read by CI, so they must not corrupt.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // The pid + counter suffix avoids collisions between concurrent writers
    // (batch workers sharing a workdir), but it is NOT unpredictable — a
    // hostile workdir can pre-place a symlink at the exact predicted name —
    // so the open is O_EXCL (`create_new`): it fails on ANY pre-existing
    // path, symlinks included (O_EXCL never follows them), where plain
    // `File::create` would write THROUGH the link and redirect the write
    // outside the workdir (write_file / edit_file land here after their own
    // target-path sandbox check, which can't see the temp sibling). On
    // collision the counter advances and a fresh suffix is tried; a stale
    // tmp from a pid-reused predecessor process is skipped the same way.
    for _ in 0..8 {
        let n = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = tmp_name(path, n);
        let mut f = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", tmp.display())),
        };
        // Write + fsync the temp file before renaming, so a crash after rename
        // never exposes a zero-length file. (result.json / report.json are read
        // by CI and must not corrupt on power loss.) A failure anywhere below
        // removes the temp file first — the rename branch already did, but a
        // write/sync failure used to leave the stray .tmp behind (visible to
        // anything globbing the directory, e.g. the next batch round's task
        // scan).
        let written = (|| -> Result<()> {
            // Preserve the existing target's permissions — above all the
            // executable bit: a fresh default-mode file would silently strip
            // -x from an edited script. Best-effort no-op when the target
            // doesn't exist yet (write_file creating a new file).
            if let Ok(meta) = std::fs::metadata(path) {
                let _ = f.set_permissions(meta.permissions());
            }
            f.write_all(bytes)
                .with_context(|| format!("cannot write {}", tmp.display()))?;
            f.sync_all()
                .with_context(|| format!("cannot sync {}", tmp.display()))?;
            Ok(())
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp); // don't litter on failure
            return Err(e).with_context(|| format!("cannot save {}", path.display()));
        }
        // fsync the parent directory so the rename's directory-entry update
        // survives a power loss. The file DATA is durable (sync_all above), but
        // on ext4 (when auto_da_alloc doesn't fire) and xfs the rename itself
        // can be lost without an explicit directory fsync — a crash could then
        // expose the pre-rename state (old content, or no file). APFS/NTFS
        // handle directory durability via their own transaction logs, so this
        // is Unix-only and the error is ignored: best-effort hardening, and
        // the rename already succeeded.
        #[cfg(unix)]
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        return Ok(());
    }
    // Every candidate name was already taken — either a stale-tmp pile-up or
    // a hostile directory planted across the whole name window.
    Err(anyhow::anyhow!(
        "cannot create a temp file next to {} (all candidate names already exist)",
        path.display()
    ))
}

fn compact(v: &serde_json::Value) -> String {
    crate::agent::preview(&v.to_string())
}

/// Streams the human-readable transcript of an agent run to two sinks:
/// `out` carries the transcript (streamed text, tool calls/results, the done
/// line, compression notices); `err` carries out-of-band chrome (turn headers,
/// token counts, warnings). Factored out of `run_with_retries` so the rendering
/// is unit-testable against in-memory sinks instead of capturing process
/// stdout/stderr (which isn't safely doable from a parallel test binary).
///
/// `at_line_start` / `current_sub_name` persist across events: streamed text
/// appends to the current line, and parallel sub-agents are tagged `[name]` and
/// broken onto their own lines so their interleaved output doesn't mash
/// together. I/O errors are ignored (best-effort): under Rust's default
/// SIG_IGN a closed downstream pipe returns `EPIPE` rather than killing us, and
/// panicking the whole run on a broken pipe is worse than silently dropping the
/// rest — the live path passes `Stdout`/`Stderr`, tests pass `Vec<u8>`.
struct HumanRenderer<Out: Write, Err: Write> {
    out: Out,
    err: Err,
    at_line_start: bool,
    current_sub_name: Option<String>,
}

impl<Out: Write, Err: Write> HumanRenderer<Out, Err> {
    fn new(out: Out, err: Err) -> Self {
        Self {
            out,
            err,
            at_line_start: true,
            current_sub_name: None,
        }
    }

    fn render(&mut self, ev: &AgentEvent) {
        match ev {
            AgentEvent::TurnStart(n) => {
                if !self.at_line_start {
                    let _ = writeln!(self.out);
                }
                let _ = writeln!(self.err, "── turn {n} ──");
                self.at_line_start = true; // we're on a fresh stdout line now
                self.current_sub_name = None;
            }
            AgentEvent::TextDelta(t) => {
                let _ = write!(self.out, "{t}");
                let _ = self.out.flush();
                self.at_line_start = t.ends_with('\n');
                self.current_sub_name = None; // parent agent owns this line now
            }
            AgentEvent::ThinkingDelta(t) => {
                // Reasoning trace → stderr only. stdout carries the assistant's
                // reply (and the --output summary is built from it), so thinking
                // must never mingle with it. Still visible on the terminal for
                // debugging, and redirectable away (2>/dev/null / separate stream
                // from --json's structured events).
                let _ = write!(self.err, "{t}");
                let _ = self.err.flush();
            }
            AgentEvent::ToolCall { name, input } => {
                let _ = writeln!(self.out, "\n[tool →] {name} {}", compact(input));
                self.at_line_start = true; // writeln ended on a fresh line
                self.current_sub_name = None;
            }
            AgentEvent::ToolResult {
                name,
                is_error,
                content,
            } => {
                let tag = if *is_error {
                    "[tool ✗]"
                } else {
                    "[tool ✓]"
                };
                // A streamed sub-agent TextDelta can leave stdout mid-line (no
                // trailing newline); ensure the tool-result row starts fresh so
                // it never sticks to the child's last line. No-op when already
                // at a line start (the common TextDelta→ToolCall→ToolResult path).
                if !self.at_line_start {
                    let _ = writeln!(self.out);
                }
                let _ = writeln!(self.out, "{tag} {name}: {}", crate::agent::preview(content));
                self.at_line_start = true;
                self.current_sub_name = None;
            }
            AgentEvent::Done { stop_reason } => {
                let _ = writeln!(self.out, "\n[done: {stop_reason}]");
                self.at_line_start = true;
                self.current_sub_name = None;
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
                let _ = writeln!(
                    self.out,
                    "\n[context compressed ({how}): dropped {dropped} messages, {kept} kept]"
                );
                self.at_line_start = true;
                self.current_sub_name = None;
            }
            AgentEvent::Warning(w) => {
                // Warnings go to stderr (stdout carries the transcript); break
                // the line first if text is mid-stream.
                if !self.at_line_start {
                    let _ = writeln!(self.out);
                    self.at_line_start = true;
                }
                let _ = writeln!(self.err, "[warn] {w}");
                self.current_sub_name = None;
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
            } => {
                let cache = if *cache_creation_input_tokens > 0 || *cache_read_input_tokens > 0 {
                    format!(
                        " · cache {}write {}read",
                        cache_creation_input_tokens, cache_read_input_tokens
                    )
                } else {
                    String::new()
                };
                let _ = writeln!(
                    self.err,
                    "[tokens: {input_tokens}↑ {output_tokens}↓{cache}]"
                );
            }
            AgentEvent::TodoUpdate { todos } => {
                // Plan progress → stderr only (stdout carries the transcript /
                // the --output summary). One compact line per update so a human
                // tailing the run sees the plan evolve; the authoritative final
                // plan is in result.json's `plan` field.
                let line = if todos.is_empty() {
                    "[plan] cleared".to_string()
                } else {
                    let total = todos.len();
                    let done = todos.iter().filter(|t| t.status == "completed").count();
                    format!("[plan] {done}/{total} done")
                };
                let _ = writeln!(self.err, "{line}");
            }
            AgentEvent::SubAgent { name, event } => {
                // Render the child's event prefixed with its identity, so a human
                // can follow each sub-agent's progress — especially when several
                // run in parallel and interleave. Usage is already folded into
                // the run totals at the agent level, so it's not re-shown here.
                match &**event {
                    AgentEvent::TextDelta(t) => {
                        // Tag a sub-agent's text with [name]. Print the tag once
                        // at the start of ITS run of text — not on every streamed
                        // chunk (else fragments render as "[scout] Hel[scout]
                        // lo[scout] world"). And if a DIFFERENT sub-agent's text
                        // arrives mid-line (parallel interleaving), break to a new
                        // line and re-tag, so their streams don't mash together.
                        let same = self.current_sub_name.as_deref() == Some(name.as_str());
                        if self.at_line_start || !same {
                            if !self.at_line_start {
                                let _ = writeln!(self.out);
                            }
                            let _ = write!(self.out, "[{name}] ");
                            self.current_sub_name = Some(name.clone());
                        }
                        let _ = write!(self.out, "{t}");
                        let _ = self.out.flush();
                        self.at_line_start = t.ends_with('\n');
                    }
                    AgentEvent::ThinkingDelta(t) => {
                        // Sub-agent reasoning → stderr, tagged with the child's
                        // name so interleaved thinking streams stay legible.
                        let _ = write!(self.err, "[{name}] {t}");
                        let _ = self.err.flush();
                    }
                    AgentEvent::ToolCall { name: tn, input } => {
                        let _ = writeln!(self.out, "\n[{name}] [tool →] {tn} {}", compact(input));
                        self.at_line_start = true;
                        self.current_sub_name = None;
                    }
                    AgentEvent::ToolResult {
                        name: tn,
                        is_error,
                        content,
                    } => {
                        let tag = if *is_error {
                            "[tool ✗]"
                        } else {
                            "[tool ✓]"
                        };
                        let _ = writeln!(
                            self.out,
                            "[{name}] {tag} {tn}: {}",
                            crate::agent::preview(content)
                        );
                        self.at_line_start = true;
                        self.current_sub_name = None;
                    }
                    AgentEvent::Done { .. } => {} // implicit in the result
                    AgentEvent::Warning(w) => {
                        if !self.at_line_start {
                            let _ = writeln!(self.out);
                            self.at_line_start = true;
                        }
                        let _ = writeln!(self.err, "[{name}] [warn] {w}");
                        self.current_sub_name = None;
                    }
                    _ => {} // TurnStart/Compressed/Usage/SubAgent: noisy or folded
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// Shared attribution meta for write_result tests.
    fn meta() -> ResultMeta {
        ResultMeta {
            model: "test-model".into(),
            provider: "openai".into(),
            started_at: "2026-08-15T07:41:02Z".into(),
        }
    }

    use super::*;
    use crate::agent::{CompressTrigger, RunOutcome, TodoItem, ToolCallRecord};
    use serde_json::json;

    #[test]
    fn atomic_write_roundtrips_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!("ta-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.json");
        atomic_write(&path, b"{\"ok\":true}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"ok\":true}");
        // The temp file must be gone after the rename (the name is now unique
        // per call, so scan the dir for any *.tmp leftover rather than a fixed
        // name).
        let no_tmp = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .all(|e| !e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(no_tmp, "temp file must be gone after rename");
        // Overwriting an existing file is also atomic.
        atomic_write(&path, b"v2").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v2");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_executable_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ta-atomic-x-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("script.sh");
        // Create the target with the executable bit set.
        std::fs::write(&path, b"#!/bin/sh\necho old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // atomic_write must carry the +x through to the new file.
        atomic_write(&path, b"#!/bin/sh\necho new\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "executable bit lost after atomic_write: mode {mode:o}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "#!/bin/sh\necho new\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_skips_preplaced_symlink_at_predicted_tmp_name() {
        // P1-1 regression: write_file/edit_file sandbox-check the TARGET path,
        // but the temp sibling name is predictable (pid + counter). A
        // pre-placed symlink at that exact name must NOT be written through —
        // the O_EXCL (`create_new`) open fails on the existing link and the
        // write retries on a fresh suffix instead of redirecting outside the
        // workdir. With plain `File::create` the victim received the bytes.
        let dir = std::env::temp_dir().join(format!("ta-atomic-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.json");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "precious").unwrap();
        // Cover a few consecutive counter values: a parallel test in this
        // process may bump TMP_COUNTER between our read and the write under
        // test.
        let n = TMP_COUNTER.load(std::sync::atomic::Ordering::Relaxed);
        for k in n..n + 3 {
            std::os::unix::fs::symlink(&victim, tmp_name(&target, k)).unwrap();
        }
        atomic_write(&target, b"{\"ok\":true}").unwrap();
        // The write landed on the target…
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"ok\":true}");
        // …and never went THROUGH any planted link: the victim is untouched
        // and the link itself still is one.
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
        assert!(
            std::fs::symlink_metadata(tmp_name(&target, n))
                .unwrap()
                .file_type()
                .is_symlink(),
            "planted symlink must not be replaced or followed"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn atomic_write_skips_stale_tmp_file_at_predicted_name() {
        // The pid-reuse case: a crashed predecessor process with our pid can
        // leave a stale `<target>.<pid>.<n>.tmp` behind. O_EXCL must skip it
        // (write to a fresh suffix) rather than truncate-write a name someone
        // else owns.
        let dir = std::env::temp_dir().join(format!("ta-atomic-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.json");
        let n = TMP_COUNTER.load(std::sync::atomic::Ordering::Relaxed);
        let stale = tmp_name(&target, n);
        std::fs::write(&stale, "stale junk from a pid-reused predecessor").unwrap();
        atomic_write(&target, b"fresh").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "fresh");
        // The stale file is inert garbage — skipped, not written through.
        assert!(
            std::fs::read_to_string(&stale)
                .unwrap()
                .starts_with("stale junk"),
            "stale tmp must not be truncated and reused"
        );
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
                meta: None,
            }],
            input_tokens: 100,
            output_tokens: 5,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            elapsed: std::time::Duration::from_secs_f64(1.5),
        };
        // Success path: tool output + token totals + summary all present.
        let path = dir.join("out.json");
        write_result(&path, Some(&o), None, 1, "error", None, &meta()).unwrap();
        let s = std::fs::read_to_string(&path).unwrap();
        assert!(
            s.contains("\"output\"") && s.contains("hello world"),
            "tool output missing"
        );
        assert!(
            s.contains("\"input_tokens\"") && s.contains("\"output_tokens\""),
            "token totals missing"
        );
        assert!(s.contains("\"summary\""));

        // Failure path: agent.run returned Err — still write a result artifact
        // (this is the gap the old `?`-skips-write_result had).
        let epath = dir.join("err.json");
        write_result(
            &epath,
            None,
            Some("provider 503"),
            1,
            "error",
            None,
            &meta(),
        )
        .unwrap();
        let es = std::fs::read_to_string(&epath).unwrap();
        assert!(es.contains("\"success\": false"));
        assert!(es.contains("provider 503"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_result_stamps_attribution_on_success_and_failure() {
        // The five attribution keys are NOT opt-in (unlike `plan`/`meta`): a
        // bill you can't attribute to a model/provider/time isn't auditable.
        // Both branches must carry them. (`line!()` keeps the temp dir unique
        // against the OTHER tests sharing this prefix pattern.)
        let dir =
            std::env::temp_dir().join(format!("ta-attrib-{}-{}", line!(), std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let keys = |s: &str| {
            for k in [
                "\"schema_version\": 1",
                "\"model\": \"test-model\"",
                "\"provider\": \"openai\"",
                "\"started_at\": \"2026-08-15T07:41:02Z\"",
                "\"finished_at\": \"",
            ] {
                assert!(s.contains(k), "missing {k} in: {s}");
            }
        };

        let o = RunOutcome {
            final_text: "done".into(),
            turns: 1,
            stop_reason: "end_turn".into(),
            tool_calls: vec![],
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            elapsed: std::time::Duration::from_secs_f64(0.1),
        };
        let path = dir.join("ok.json");
        write_result(&path, Some(&o), None, 1, "error", None, &meta()).unwrap();
        keys(&std::fs::read_to_string(&path).unwrap());

        let epath = dir.join("fail.json");
        write_result(&epath, None, Some("boom"), 1, "error", None, &meta()).unwrap();
        keys(&std::fs::read_to_string(&epath).unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rfc3339_utc_formats_known_instants() {
        use std::time::{Duration, UNIX_EPOCH};
        let at = |secs: u64| rfc3339_utc(UNIX_EPOCH + Duration::from_secs(secs));
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        // 2026-08-15T00:00:00Z (verified independently).
        assert_eq!(at(1_786_752_000), "2026-08-15T00:00:00Z");
        // Leap day 2024-02-29T12:00:00Z.
        assert_eq!(at(1_709_208_000), "2024-02-29T12:00:00Z");
        // Year boundary: the last second of 2023.
        assert_eq!(at(1_704_067_199), "2023-12-31T23:59:59Z");
        // First second of a leap year: 2024-01-01T00:00:00Z.
        assert_eq!(at(1_704_067_200), "2024-01-01T00:00:00Z");
        // Day rollover mid-run: 23:59:59 → next day 00:00:00.
        assert_eq!(at(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(at(86_400), "1970-01-02T00:00:00Z");
    }

    #[test]
    fn write_result_emits_plan_when_todo_used_and_omits_otherwise() {
        // The `plan` key is conditional: present (derived from the last `todo`
        // call) when the agent tracked a plan, absent otherwise — so a result
        // with no plan is byte-identical to pre-feature for consumers that
        // don't know the field.
        let dir = std::env::temp_dir().join(format!("ta-plan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Outcome WITH a todo call → `plan` present.
        let with_plan = RunOutcome {
            final_text: "done".into(),
            turns: 1,
            stop_reason: "end_turn".into(),
            tool_calls: vec![
                ToolCallRecord {
                    name: "todo".into(),
                    input: json!({"todos":[
                        {"subject":"a","status":"completed"},
                        {"subject":"b","status":"in_progress"}
                    ]}),
                    output: "☐ ...".into(),
                    is_error: false,
                    meta: None,
                },
                ToolCallRecord {
                    name: "shell".into(),
                    input: json!({"command":"ls"}),
                    output: "x".into(),
                    is_error: false,
                    meta: None,
                },
            ],
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            elapsed: std::time::Duration::from_secs_f64(1.0),
        };
        let path = dir.join("plan.json");
        write_result(&path, Some(&with_plan), None, 1, "error", None, &meta()).unwrap();
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let obj = parsed.as_object().expect("result is a JSON object");
        assert!(obj.contains_key("plan"), "plan key present when todo used");
        assert_eq!(parsed["plan"][0]["status"], "completed");
        assert_eq!(parsed["plan"][1]["subject"], "b");

        // Outcome with NO todo call → `plan` key absent (not null).
        let without_plan = RunOutcome {
            final_text: "done".into(),
            turns: 1,
            stop_reason: "end_turn".into(),
            tool_calls: vec![ToolCallRecord {
                name: "shell".into(),
                input: json!({"command":"ls"}),
                output: "x".into(),
                is_error: false,
                meta: None,
            }],
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            elapsed: std::time::Duration::from_secs_f64(1.0),
        };
        let np = dir.join("noplan.json");
        write_result(&np, Some(&without_plan), None, 1, "error", None, &meta()).unwrap();
        let nparsed: Value = serde_json::from_str(&std::fs::read_to_string(&np).unwrap()).unwrap();
        assert!(
            !nparsed.as_object().unwrap().contains_key("plan"),
            "plan key absent when no todo used"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_result_emits_tool_meta_only_when_present() {
        // `meta` is sparse (only `task` sets it): a tool_call with meta surfaces
        // it, one without stays byte-identical (no null meta key).
        let dir = std::env::temp_dir().join(format!("ta-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let o = RunOutcome {
            final_text: "done".into(),
            turns: 1,
            stop_reason: "end_turn".into(),
            tool_calls: vec![
                ToolCallRecord {
                    name: "task".into(),
                    input: json!({"name":"scout","prompt":"x"}),
                    output: "summary".into(),
                    is_error: false,
                    meta: Some(
                        json!({"turns":2,"tools":{"shell":1},"tokens":{"in":50,"out":5},"stop_reason":"end_turn"}),
                    ),
                },
                ToolCallRecord {
                    name: "shell".into(),
                    input: json!({"command":"ls"}),
                    output: "x".into(),
                    is_error: false,
                    meta: None,
                },
            ],
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            elapsed: std::time::Duration::from_secs_f64(1.0),
        };
        let path = dir.join("meta.json");
        write_result(&path, Some(&o), None, 1, "error", None, &meta()).unwrap();
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // The task entry carries its trace meta.
        assert_eq!(parsed["tool_calls"][0]["name"], "task");
        assert_eq!(parsed["tool_calls"][0]["meta"]["turns"], 2);
        assert_eq!(parsed["tool_calls"][0]["meta"]["tools"]["shell"], 1);
        // The shell entry has NO meta key (sparse — not null).
        let shell_obj = parsed["tool_calls"][1]
            .as_object()
            .expect("shell entry is an object");
        assert!(
            !shell_obj.contains_key("meta"),
            "leaf tool has no meta key: {:?}",
            shell_obj
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_result_failure_path_keeps_accumulated_cost() {
        // P2: a run that fails after burning tokens must still report them. The
        // final-failure paths (retries exhausted, cancel during backoff) call
        // write_result with outcome=None and the accumulated RunCost — the
        // result.json must fold that cost in, not silently zero everything.
        let dir = std::env::temp_dir().join(format!("ta-res-acc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Two failed attempts' worth of tokens + a tool call, as RunCost would
        // hold after two add_failed() calls.
        let acc = RunCost {
            usage: crate::provider::Usage {
                input_tokens: 300,
                output_tokens: 40,
                cache_creation_input_tokens: 100,
                cache_read_input_tokens: 50,
            },
            tool_calls: vec![ToolCallRecord {
                name: "shell".into(),
                input: json!({"command": "ls"}),
                output: "a.txt".into(),
                is_error: false,
                meta: None,
            }],
            turns: 3,
            elapsed: std::time::Duration::from_secs(12),
        };
        let path = dir.join("fail.json");
        write_result(
            &path,
            None,
            Some("provider 503"),
            2,
            "error",
            Some(&acc),
            &meta(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // Failure semantics.
        assert_eq!(parsed["success"], false);
        assert_eq!(parsed["stop_reason"], "error");
        assert_eq!(parsed["attempts"], 2);
        assert_eq!(parsed["error"], "provider 503");
        // Honest cost: the accumulated tokens survive into the failure record.
        assert_eq!(parsed["input_tokens"], 300);
        assert_eq!(parsed["output_tokens"], 40);
        assert_eq!(parsed["cache_creation_input_tokens"], 100);
        assert_eq!(parsed["cache_read_input_tokens"], 50);
        // turns/elapsed_secs/summary are present on the failure path too, so
        // CI consumers reading result.json get a stable schema regardless of
        // success (the failure path used to omit these keys entirely).
        assert_eq!(parsed["turns"], 3);
        assert_eq!(parsed["elapsed_secs"], 12.0);
        assert_eq!(parsed["summary"], "");
        // And the tool call from the failed attempt is recorded too.
        assert_eq!(parsed["tool_calls"][0]["name"], "shell");
        assert_eq!(parsed["tool_calls"][0]["output"], "a.txt");

        // No accumulated cost (e.g. a setup error before any turn) ⇒ zeros,
        // never missing keys (schema stays stable for CI consumers).
        let zpath = dir.join("zero.json");
        write_result(
            &zpath,
            None,
            Some("setup failed"),
            1,
            "error",
            None,
            &meta(),
        )
        .unwrap();
        let zparsed: Value =
            serde_json::from_str(&std::fs::read_to_string(&zpath).unwrap()).unwrap();
        assert_eq!(zparsed["input_tokens"], 0);
        assert_eq!(zparsed["output_tokens"], 0);
        assert_eq!(zparsed["tool_calls"].as_array().unwrap().len(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exit_code_distinguishes_outcomes() {
        assert_eq!(exit_code_for("end_turn"), 0);
        assert_eq!(exit_code_for("timeout"), 124);
        assert_eq!(exit_code_for("budget_exceeded"), 125);
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
        // TodoUpdate → {"type":"todo","todos":[{subject,status},...]} — the
        // full plan rides on every update (full-replacement semantics), so a
        // consumer can render the checklist from any single event.
        let v = event_json(&AgentEvent::TodoUpdate {
            todos: vec![
                TodoItem {
                    subject: "a".into(),
                    status: "completed".into(),
                },
                TodoItem {
                    subject: "b".into(),
                    status: "in_progress".into(),
                },
            ],
        });
        assert_eq!(v["type"], "todo");
        assert_eq!(v["todos"][0]["subject"], "a");
        assert_eq!(v["todos"][0]["status"], "completed");
        assert_eq!(v["todos"][1]["status"], "in_progress");
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
        let base = std::env::temp_dir().join(format!("ta-atomic-parent-{}", std::process::id()));
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
        write_result(&path, Some(&o), None, 3, "error", None, &meta()).unwrap();
        // Parse back and assert by field name — a typo in the cache_* names
        // would silently drop cache cost from downstream accounting, and
        // substring checks couple to pretty-print spacing.
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["cache_creation_input_tokens"], 800);
        assert_eq!(parsed["cache_read_input_tokens"], 200);
        assert_eq!(parsed["attempts"], 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn renderer_routes_transcript_to_out_and_chrome_to_err() {
        let mut r: HumanRenderer<Vec<u8>, Vec<u8>> = HumanRenderer::new(Vec::new(), Vec::new());

        // Chrome (out-of-band) → stderr.
        r.render(&AgentEvent::TurnStart(1));
        r.render(&AgentEvent::Warning("careful".into()));
        r.render(&AgentEvent::Usage {
            input_tokens: 10,
            output_tokens: 2,
            cache_creation_input_tokens: 5,
            cache_read_input_tokens: 8,
        });
        let err_s = String::from_utf8_lossy(&r.err).into_owned();
        assert!(err_s.contains("turn 1"));
        assert!(err_s.contains("[warn] careful"));
        assert!(
            err_s.contains("[tokens: 10↑ 2↓ · cache 5write 8read]"),
            "cache suffix format: {err_s}"
        );

        // Transcript → stdout.
        r.render(&AgentEvent::TextDelta("hello".into()));
        r.render(&AgentEvent::ToolCall {
            name: "shell".into(),
            input: json!({"command": "ls"}),
        });
        r.render(&AgentEvent::ToolResult {
            name: "shell".into(),
            is_error: false,
            content: "ok".into(),
        });
        r.render(&AgentEvent::ToolResult {
            name: "shell".into(),
            is_error: true,
            content: "boom".into(),
        });
        r.render(&AgentEvent::Compressed {
            dropped: 3,
            kept: 5,
            trigger: CompressTrigger::Preflight,
        });
        r.render(&AgentEvent::Done {
            stop_reason: "end_turn".into(),
        });
        let out_s = String::from_utf8_lossy(&r.out).into_owned();
        assert!(out_s.contains("hello"));
        assert!(out_s.contains("[tool →] shell"));
        assert!(out_s.contains("[tool ✓] shell"));
        assert!(out_s.contains("[tool ✗] shell"));
        assert!(out_s.contains("[context compressed (preflight): dropped 3 messages, 5 kept]"));
        assert!(out_s.contains("[done: end_turn]"));
        // Chrome must not leak into the transcript.
        assert!(!out_s.contains("[tokens:") && !out_s.contains("[warn]"));
    }

    #[test]
    fn renderer_starts_tool_result_on_a_fresh_line_after_subagent_text() {
        // Regression: a sub-agent TextDelta with no trailing newline left stdout
        // mid-line; the parent's ToolResult then stuck to it ("...result[tool ✓]
        // task:"). The ToolResult arm must break to a new line when not
        // at_line_start (mirrors the warn arm).
        let mut r: HumanRenderer<Vec<u8>, Vec<u8>> = HumanRenderer::new(Vec::new(), Vec::new());
        r.render(&AgentEvent::SubAgent {
            name: "scout".into(),
            event: Box::new(AgentEvent::TextDelta("partial-result".into())),
        });
        r.render(&AgentEvent::ToolResult {
            name: "task".into(),
            is_error: false,
            content: "done".into(),
        });
        let out_s = String::from_utf8_lossy(&r.out).into_owned();
        assert!(
            out_s.contains("partial-result\n[tool ✓] task:"),
            "tool result must start on a fresh line, got: {out_s}"
        );
        assert!(
            !out_s.contains("partial-result[tool ✓]"),
            "sub-agent text must not stick to the tool-result row, got: {out_s}"
        );
    }

    #[test]
    fn renderer_tags_interleaved_subagent_streams_on_separate_lines() {
        fn sub_text(name: &str, t: &str) -> AgentEvent {
            AgentEvent::SubAgent {
                name: name.into(),
                event: Box::new(AgentEvent::TextDelta(t.into())),
            }
        }
        let mut r: HumanRenderer<Vec<u8>, Vec<u8>> = HumanRenderer::new(Vec::new(), Vec::new());
        // scout streams two contiguous chunks → tagged once at the start of its run.
        r.render(&sub_text("scout", "Hel"));
        r.render(&sub_text("scout", "lo"));
        // A different sub-agent mid-line breaks to a new line and re-tags.
        r.render(&sub_text("worker", "x"));
        let out_s = String::from_utf8_lossy(&r.out).into_owned();
        assert_eq!(
            out_s.matches("[scout]").count(),
            1,
            "tag should print once per contiguous run, not per chunk: {out_s}"
        );
        assert!(
            out_s.contains("[scout] Hello"),
            "scout's chunks should join: {out_s}"
        );
        assert!(
            out_s.contains("[worker] x"),
            "second agent on its own tagged line: {out_s}"
        );
    }
}
