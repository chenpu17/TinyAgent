use crate::agent::RunCost;
use crate::config::{Config, PartialConfig};
use crate::harness;
use crate::retry::backoff;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Mutex};

#[derive(Debug, Clone, Deserialize)]
struct TaskReport {
    name: String,
    success: bool,
    stop_reason: String,
    turns: u32,
    elapsed_secs: f64,
    error: Option<String>,
    // Token totals for the task (honest cost across all attempts, P2). A failed
    // task that burned tokens before erroring reports them here, not zero —
    // otherwise report.json undercounts a batch's real spend. `#[serde(default)]`
    // so prior reports written before these fields existed still parse for
    // `--failed` (missing ⇒ treated as 0).
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

/// Run every `*.json` task fragment in `dir` against the base config,
/// with a fixed pool of worker threads.
/// - `retries`: extra attempts per task on transient failures only.
/// - `failed`: re-run only tasks that failed in the prior report at `report_path`.
/// - `cancel`: shared flag flipped by SIGINT (headless). Once set, workers stop
///   dispatching new tasks — in-flight tasks finish on their own, not-yet-started
///   ones are marked `cancelled` so the report still accounts for every task.
///
/// Returns process exit code (130 if cancelled, else 0/1 by success).
pub fn run(
    base: &Config,
    dir: &Path,
    jobs: usize,
    report_path: &Path,
    retries: usize,
    failed: bool,
    cancel: Arc<std::sync::atomic::AtomicBool>,
) -> Result<i32> {
    // If --report points at a *.json inside the batch dir, a re-run would pick
    // up the prior report.json as a task fragment (it's a *.json sitting in
    // `dir`). Canonicalize lets us skip exactly that file when it already
    // exists; on a first run the report isn't there yet, so nothing matches.
    let report_canon = report_path.canonicalize().ok();
    let mut files: Vec<PathBuf> = Vec::new();
    for e in std::fs::read_dir(dir)
        .with_context(|| format!("cannot read batch dir: {}", dir.display()))?
    {
        let p = e?.path();
        if p.extension().and_then(|s| s.to_str()) == Some("json") {
            if matches!(&report_canon, Some(rc) if p.canonicalize().ok().as_ref() == Some(rc)) {
                continue;
            }
            files.push(p);
        }
    }
    files.sort();
    if files.is_empty() {
        bail!("no *.json task files in {}", dir.display());
    }

    // Parse all task fragments up front (fail fast on bad input).
    let mut tasks: Vec<(String, PartialConfig)> = Vec::new();
    for f in files {
        let raw = std::fs::read_to_string(&f)?;
        let partial: PartialConfig = serde_json::from_str(&raw)
            .with_context(|| format!("invalid task file: {}", f.display()))?;
        let name = f
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "task".into());
        tasks.push((name, partial));
    }

    // --failed: re-run only tasks that failed in the prior report (at the same
    // --report path). We load the whole prior report once (before overwriting
    // it) so we can both filter to failed tasks AND, at write time, merge the
    // fresh results back — a re-run that now passes folds into a report that
    // still reflects the full task set, not just the re-run subset.
    let prior: Option<Vec<TaskReport>> = if failed {
        let prior_tasks = read_prior_tasks(report_path)?;
        let failed_names: std::collections::HashSet<String> = prior_tasks
            .iter()
            .filter(|t| !t.success)
            .map(|t| t.name.clone())
            .collect();
        // Count the prior successes that actually survive the merge (a success
        // whose name also appears among the failures gets overlaid). Plain
        // `prior.len() - failed_names.len()` miscounts when a name repeats
        // (HashSet dedups, the Vec doesn't).
        let kept_successes = prior_tasks
            .iter()
            .filter(|t| t.success && !failed_names.contains(&t.name))
            .count();
        tasks.retain(|(name, _)| failed_names.contains(name));
        eprintln!(
            "batch --failed: re-running {} failed task(s); {} prior success(es) kept in report",
            tasks.len(),
            kept_successes
        );
        if tasks.is_empty() {
            bail!("no previously-failed tasks matched files in {}", dir.display());
        }
        Some(prior_tasks)
    } else {
        None
    };

    let results_dir = report_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .join("results");
    std::fs::create_dir_all(&results_dir)?;

    // One HTTP client (connection pool) shared by all tasks. Its timeout is
    // the BASE limit: a task fragment can only TIGHTEN timeout (config merges
    // with min, and timeout_secs==0 means "no override" — see config.rs), so
    // every task's final timeout is ≤ base. The base is already the loosest
    // upper bound covering every task; folding the fragments' raw values in
    // was dead logic that also let a fragment 0 ("no override") punch through
    // to no-timeout, pinning a worker on a hung read.
    let max_timeout = base.limits.timeout_secs;
    let client = crate::provider::new_client(max_timeout);

    eprintln!(
        "batch: {} tasks, {} workers, {} retries, results → {}",
        tasks.len(),
        jobs,
        retries,
        results_dir.display()
    );
    // Concurrent workers all share base.workdir (task fragments can't override
    // it), so jobs>1 with write-file tasks can corrupt each other's results.
    // Surface this at startup so it's an informed choice, not a silent trap.
    // (Per-task workdir isolation would need a copy per task — impractical for
    // large repos — so the contract is: shared by default, user opts into jobs=1
    // when tasks write overlapping paths.)
    if jobs > 1 {
        let wd = base
            .workdir
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<current dir>".to_string());
        eprintln!(
            "note: --jobs {jobs} shares one workdir ({wd}); tasks writing the same \
             files can corrupt each other — use --jobs 1 if tasks write overlapping \
             paths."
        );
    }

    let (tx, rx) = mpsc::channel::<(String, PartialConfig)>();
    let rx = Mutex::new(rx);
    let reports: Mutex<Vec<TaskReport>> = Mutex::new(Vec::new());

    // Worker Mutexes tolerate poisoning (.unwrap_or_else(into_inner), matching
    // mcp.rs): a panic while holding `rx`/`reports` unwinds out of the scope
    // (debug panic="unwind") or aborts outright (release panic="abort"); into_inner
    // keeps a surviving thread from re-panicking on a poisoned lock. NB: under
    // panic="abort" a worker panic aborts the process immediately regardless of
    // lock state, so this is a debug/CI-robustness + consistency measure, not a
    // release-abort guard — a panicking worker still loses the report in release.
    std::thread::scope(|s| {
        for _ in 0..jobs.max(1) {
            s.spawn(|| loop {
                let next = rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                let (name, partial) = match next {
                    Ok(t) => t,
                    Err(_) => return, // channel drained
                };
                let report = if cancel.load(Ordering::Relaxed) {
                    // Ctrl-C was hit: stop dispatching new tasks. Mark this one
                    // cancelled (not run) so the report still accounts for every
                    // task. In-flight tasks in other workers finish on their own
                    // — we don't hard-interrupt a running agent here (sharing one
                    // cancel flag across workers would race with agent::run()'s
                    // entry reset); a second Ctrl-C force-exits via _exit(130).
                    TaskReport {
                        name,
                        success: false,
                        stop_reason: "cancelled".into(),
                        turns: 0,
                        elapsed_secs: 0.0,
                        error: Some("cancelled by user (not started)".into()),
                        input_tokens: 0,
                        output_tokens: 0,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }
                } else {
                    run_one(base, &name, partial, &results_dir, &client, retries, &cancel)
                };
                eprintln!(
                    "[{}] {} ({} turns, {:.1}s{})",
                    report.name,
                    if report.success { "✓" } else { "✗" },
                    report.turns,
                    report.elapsed_secs,
                    report
                        .error
                        .as_deref()
                        .map(|e| format!(", {e}"))
                        .unwrap_or_default()
                );
                reports.lock().unwrap_or_else(|e| e.into_inner()).push(report);
            });
        }
        for t in tasks {
            let _ = tx.send(t);
        }
        drop(tx); // workers exit once the queue is drained
    });

    let mut reports = reports.into_inner().unwrap_or_else(|e| e.into_inner());
    reports.sort_by(|a, b| a.name.cmp(&b.name));
    // For --failed: overlay the re-run results onto the prior report so the
    // written file reflects the full task set (prior successes preserved,
    // re-run tasks updated). A plain run just writes its own results.
    let final_reports = match prior {
        Some(p) => merge_reports(p, &reports),
        None => reports,
    };
    let succeeded = final_reports.iter().filter(|r| r.success).count();
    write_report(report_path, &final_reports)?;
    eprintln!(
        "batch done: {}/{} succeeded, report → {}",
        succeeded,
        final_reports.len(),
        report_path.display()
    );
    Ok(if cancel.load(Ordering::Relaxed) {
        130 // cancelled by Ctrl-C — report still written above
    } else if succeeded == final_reports.len() {
        0
    } else {
        1
    })
}

/// One attempt at a task. Returns a `stop_reason` that signals retryability:
/// `"error"` is transient (agent.run returned Err — provider/network) and is
/// retried by `run_one`; `"bad_task"` is a deterministic config problem
/// (forbidden override, bad JSON, no task) and `"setup_error"` a deterministic
/// environment problem (missing api_key_env, MCP server won't start) — neither
/// is retried; any other value is the model's real stop reason (end_turn /
/// max_turns / …) and is not retried either.
fn run_one_attempt(
    base: &Config,
    name: &str,
    partial: &PartialConfig,
    results_dir: &Path,
    client: &reqwest::blocking::Client,
    // 1-based attempt index, written to result.json's `attempts` so CI can
    // see how many tries a retried task actually took (was hardcoded 1).
    attempt_no: usize,
    // Accumulated partial cost from earlier FAILED attempts (tokens burned +
    // tools run, whose side effects already landed). Folded into this
    // attempt's outcome on success so result.json bills every attempt
    // honestly (P2-3); extended on a transient error for the next try.
    acc: &mut RunCost,
) -> TaskReport {
    let start = std::time::Instant::now();
    // `usage` is the token cost to bill this report. bad_task / setup_error burn
    // nothing (zero, turns=0); the transient-error path bills the accumulated
    // cost across attempts (passed in from the caller after `acc.add_failed`),
    // including the turns those failed attempts actually completed.
    let bad =
        |stop: &str, msg: String, usage: &crate::provider::Usage, turns: u32| TaskReport {
            name: name.into(),
            success: false,
            stop_reason: stop.into(),
            turns,
            elapsed_secs: start.elapsed().as_secs_f64(),
            error: Some(msg),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
        };
    let zero = crate::provider::Usage::default();

    let mut cfg = base.clone();
    if let Some(field) = partial.forbidden_batch_override() {
        return bad(
            "bad_task",
            format!(
                "task file must not override '{field}' (only task/system_prompt/model/max_tokens/limits \
                 are overridable in batch mode)"
            ),
            &zero,
            0,
        );
    }
    if let Err(e) = cfg.apply(partial.clone()) {
        return bad("bad_task", format!("{e:#}"), &zero, 0);
    }
    if cfg.task.is_none() {
        return bad(
            "bad_task",
            "task file did not set \"task\" and base config has none".into(),
            &zero,
            0,
        );
    }

    // Agent construction failures are DETERMINISTIC — a missing api_key_env,
    // an MCP server that won't start, an unknown tool name — retrying them
    // under --retries just burns backoff time to fail identically. Split them
    // out of the transient "error" class so run_one doesn't retry them.
    let task = cfg.task.clone().expect("checked above");
    let initial = vec![crate::provider::Message::user(vec![
        crate::provider::Block::Text(task),
    ])];
    let mut agent = match crate::build_agent(&cfg, initial, client) {
        Ok(a) => a,
        Err(e) => return bad("setup_error", format!("agent setup failed: {e:#}"), &zero, 0),
    };

    let outcome = agent.run(&mut |_| {}); // quiet: no per-event printing

    match outcome {
        Ok(mut o) => {
            // Bill every attempt: fold the failed attempts' accumulated cost
            // into this outcome before writing, so result.json's token totals
            // and tool-call list cover the whole task, not just the last try.
            acc.fold_into(&mut o);
            // A failed result write must not be swallowed: CI reads this file
            // for the full tool_calls/summary. Surface it so report.json does
            // not claim success while the result file is missing.
            let mut error = None;
            if let Err(e) =
                harness::write_result(&results_dir.join(format!("{name}.json")), Some(&o), None, attempt_no, "error", None)
            {
                eprintln!("[warn] {name}: failed to write result file: {e:#}");
                error = Some(format!("result file not written: {e:#}"));
            }
            let success = error.is_none() && o.stop_reason == "end_turn";
            TaskReport {
                name: name.into(),
                success,
                stop_reason: o.stop_reason,
                turns: o.turns,
                elapsed_secs: o.elapsed.as_secs_f64(),
                error,
                input_tokens: o.input_tokens,
                output_tokens: o.output_tokens,
                cache_creation_input_tokens: o.cache_creation_input_tokens,
                cache_read_input_tokens: o.cache_read_input_tokens,
            }
        }
        Err(re) => {
            // This attempt failed transiently, but it already burned tokens
            // and ran tools — accumulate them so the next attempt (if any)
            // bills them, and so a final success isn't undercosted.
            acc.add_failed(&re);
            let msg = format!("{re:#}");
            // Write a per-task result file for the failure too — audit parity
            // with single-run (which always produces a result artifact). The
            // accumulated tokens + tool_calls land here via write_result's None
            // branch, so a failed batch task's tool calls are recoverable from
            // results/<name>.json, not just the token totals in report.json. A
            // later successful attempt overwrites this; on all-fail the last
            // attempt's record is what remains. (P2 follow-up: batch failures
            // previously wrote no result file at all.)
            if let Err(e) = harness::write_result(
                &results_dir.join(format!("{name}.json")),
                None,
                Some(&msg),
                attempt_no,
                "error",
                // `acc` is a &mut param here, so pass it directly (an implicit
                // shared reborrow to &RunCost) — not `&acc` (a needless double
                // borrow). The later `&acc.usage` still borrows fine.
                Some(acc),
            ) {
                eprintln!("[warn] {name}: failed to write result file: {e:#}");
            }
            // Bill the accumulated cost (this attempt + any prior retries) so a
            // failed task reports honest token totals in report.json, not zero.
            bad("error", msg, &acc.usage, acc.turns)
        }
    }
}

/// Run a task, retrying up to `retries` extra times on transient (`"error"`)
/// failures with exponential backoff. Successes and deterministic failures
/// (bad_task, max_turns, …) are not retried — they won't change on a re-run.
/// A Ctrl-C during a backoff (mirroring single-run `run_with_retries`) marks
/// the task cancelled instead of running another full attempt: the worker's
/// per-task gate only stops the NEXT task, so without this the cancel is
/// swallowed until the in-flight retry loop finishes.
fn run_one(
    base: &Config,
    name: &str,
    partial: PartialConfig,
    results_dir: &Path,
    client: &reqwest::blocking::Client,
    retries: usize,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
) -> TaskReport {
    let start = std::time::Instant::now();
    // Honest-cost accumulator across attempts (P2-3): a fresh agent is built per
    // attempt, so each failed attempt's tokens/tools can only be recovered via
    // the RunError it returns — accumulate here and fold into the final result.
    let mut acc = RunCost::default();
    let mut report = run_one_attempt(base, name, &partial, results_dir, client, 1, &mut acc);
    let mut attempt = 0;
    while report.stop_reason == "error" && attempt < retries {
        let backoff = backoff(attempt);
        eprintln!(
            "[{}] transient error ({}); retry {}/{} in {:?}",
            name,
            report.error.as_deref().unwrap_or(""),
            attempt + 1,
            retries,
            backoff,
        );
        // Sleep the backoff in ~100ms slices so a Ctrl-C during it (up to ~40s
        // with jitter) is honored within ~100ms instead of after the whole
        // sleep — otherwise the next run_one_attempt runs a full task the user
        // wanted to abort, and only the gate on the NEXT task stops the batch.
        if crate::retry::sleep_cancelable(cancel.as_ref(), backoff) {
            report = TaskReport {
                name: name.into(),
                success: false,
                stop_reason: "cancelled".into(),
                turns: acc.turns,
                elapsed_secs: start.elapsed().as_secs_f64(),
                error: Some("cancelled by user during retry backoff".into()),
                // Preserve the tokens burned by the failed attempts before the
                // cancel (P2): a cancelled retry still cost real money.
                input_tokens: acc.usage.input_tokens,
                output_tokens: acc.usage.output_tokens,
                cache_creation_input_tokens: acc.usage.cache_creation_input_tokens,
                cache_read_input_tokens: acc.usage.cache_read_input_tokens,
            };
            break;
        }
        attempt += 1;
        report = run_one_attempt(base, name, &partial, results_dir, client, attempt + 1, &mut acc);
    }
    // run_one_attempt stamps per-attempt elapsed; overwrite with the total
    // across attempts + backoffs so report.json reflects real wall cost.
    report.elapsed_secs = start.elapsed().as_secs_f64();
    report
}

/// Read the `tasks` array out of a prior batch report. Used by `--failed` both
/// to pick the failed subset and as the merge base for the updated report.
fn read_prior_tasks(path: &Path) -> Result<Vec<TaskReport>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read prior report {}", path.display()))?;
    // serde will reject valid JSON that lacks a "tasks" array (the field is
    // non-optional), which is exactly the "not a real report" case.
    let file: ReportFile =
        serde_json::from_str(&raw).context("prior report is not a valid batch report")?;
    Ok(file.tasks)
}

/// Names of tasks that failed in a prior batch report (for `--failed`). A
/// failure with no name (or empty) is ignored — it can't be matched to a file.
#[cfg(test)]
fn failed_names_from_report(path: &Path) -> Result<Vec<String>> {
    Ok(read_prior_tasks(path)?
        .into_iter()
        .filter(|t| !t.success)
        .map(|t| t.name)
        .filter(|s| !s.is_empty())
        .collect())
}

/// Overlay `fresh` re-run results onto `prior`, keyed by task name. A task
/// present in `fresh` replaces its prior entry (the re-run is now the truth of
/// record); everything else from `prior` — the successes we didn't re-run — is
/// kept verbatim. Result is sorted by name.
fn merge_reports(prior: Vec<TaskReport>, fresh: &[TaskReport]) -> Vec<TaskReport> {
    let mut by_name: std::collections::HashMap<String, TaskReport> =
        prior.into_iter().map(|t| (t.name.clone(), t)).collect();
    for t in fresh {
        by_name.insert(t.name.clone(), t.clone());
    }
    let mut out: Vec<_> = by_name.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Shape of a report.json file on disk — only `tasks` is read back; the
/// total/succeeded/failed summary is recomputed when we write.
#[derive(Deserialize)]
struct ReportFile {
    tasks: Vec<TaskReport>,
}

fn write_report(path: &Path, reports: &[TaskReport]) -> Result<()> {
    let tasks: Vec<_> = reports
        .iter()
        .map(|r| {
            json!({
                "name": r.name,
                "success": r.success,
                "stop_reason": r.stop_reason,
                "turns": r.turns,
                "elapsed_secs": r.elapsed_secs,
                "input_tokens": r.input_tokens,
                "output_tokens": r.output_tokens,
                "cache_creation_input_tokens": r.cache_creation_input_tokens,
                "cache_read_input_tokens": r.cache_read_input_tokens,
                "error": r.error,
            })
        })
        .collect();
    let succeeded = reports.iter().filter(|r| r.success).count();
    // Top-level token aggregates — the "bill" view. Per-task totals already
    // live in `tasks[]`; these sums (and per-task averages) let a CI pipeline
    // read a batch's real spend at a glance instead of summing the array after
    // each `--failed` iteration. All four token axes are summed; cache_read is
    // included because it still reflects tokens moved over the wire.
    let (mut ti, mut to, mut tcc, mut tcr) = (0u64, 0u64, 0u64, 0u64);
    for r in reports {
        ti += r.input_tokens;
        to += r.output_tokens;
        tcc += r.cache_creation_input_tokens;
        tcr += r.cache_read_input_tokens;
    }
    // per-task average; 0 when there are no tasks (avoids div-by-zero).
    let avg = |t: u64| if reports.is_empty() { 0.0 } else { t as f64 / reports.len() as f64 };
    let report = json!({
        "total": reports.len(),
        "succeeded": succeeded,
        "failed": reports.len() - succeeded,
        "totals": {
            "input_tokens": ti,
            "output_tokens": to,
            "cache_creation_input_tokens": tcc,
            "cache_read_input_tokens": tcr,
        },
        "averages": {
            "input_tokens": avg(ti),
            "output_tokens": avg(to),
            "cache_creation_input_tokens": avg(tcc),
            "cache_read_input_tokens": avg(tcr),
        },
        "tasks": tasks,
    });
    let bytes = serde_json::to_string_pretty(&report)?;
    harness::atomic_write(path, bytes.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    /// Recorded [start, end] window for each request the mock served — used to
    /// prove two batch tasks' provider calls actually overlapped in wall clock.
    type Spans = Arc<Mutex<Vec<(Instant, Instant)>>>;

    #[test]
    fn failed_names_from_report_selects_failures() {
        let dir = std::env::temp_dir().join(format!("ta-batch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.json");
        // A realistic prior report (write_report's exact shape): a ok, b/c
        // failed, plus a failed task with an empty name — the latter can't be
        // matched to a file, so it must be dropped from the re-run list.
        let reports = vec![tr("a", true), tr("b", false), tr("c", false), tr("", false)];
        write_report(&path, &reports).unwrap();

        let names = failed_names_from_report(&path).unwrap();
        // Only the failed tasks with names; the empty-name one is dropped.
        assert_eq!(names, vec!["b".to_string(), "c".to_string()]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failed_names_from_report_rejects_bad_report() {
        let dir = std::env::temp_dir().join(format!("ta-batch-err-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Missing file.
        let missing = dir.join("nope.json");
        assert!(failed_names_from_report(&missing).is_err());

        // Not JSON.
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "not json").unwrap();
        assert!(failed_names_from_report(&bad).is_err());

        // Valid JSON but no tasks array.
        let notasks = dir.join("notasks.json");
        std::fs::write(&notasks, r#"{"total":0}"#).unwrap();
        assert!(failed_names_from_report(&notasks).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    // Minimal TaskReport builder for the merge/read tests below.
    fn tr(name: &str, success: bool) -> TaskReport {
        TaskReport {
            name: name.into(),
            success,
            stop_reason: if success { "end_turn" } else { "error" }.into(),
            turns: 1,
            elapsed_secs: 1.0,
            error: if success { None } else { Some("e".into()) },
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    }

    // Like `tr` but with explicit token totals, for the aggregation test.
    fn tr_tok(name: &str, success: bool, i: u64, o: u64, cc: u64, cr: u64) -> TaskReport {
        let mut r = tr(name, success);
        r.input_tokens = i;
        r.output_tokens = o;
        r.cache_creation_input_tokens = cc;
        r.cache_read_input_tokens = cr;
        r
    }

    #[test]
    fn write_report_aggregates_token_totals_and_averages() {
        let dir = std::env::temp_dir().join(format!("ta-batch-agg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.json");
        let reports = vec![
            tr_tok("a", true, 100, 10, 5, 20),
            tr_tok("b", false, 200, 30, 15, 80),
        ];
        write_report(&path, &reports).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // Top-level totals sum every token axis across tasks.
        let totals = v.get("totals").unwrap();
        assert_eq!(totals["input_tokens"].as_u64().unwrap(), 300);
        assert_eq!(totals["output_tokens"].as_u64().unwrap(), 40);
        assert_eq!(totals["cache_creation_input_tokens"].as_u64().unwrap(), 20);
        assert_eq!(totals["cache_read_input_tokens"].as_u64().unwrap(), 100);
        // Averages are per-task (total / task count).
        let averages = v.get("averages").unwrap();
        assert_eq!(averages["input_tokens"].as_f64().unwrap(), 150.0);
        assert_eq!(averages["output_tokens"].as_f64().unwrap(), 20.0);
        assert_eq!(averages["cache_read_input_tokens"].as_f64().unwrap(), 50.0);
        // Per-task totals in tasks[] are unchanged.
        assert_eq!(v["tasks"][0]["input_tokens"].as_u64().unwrap(), 100);
        assert_eq!(v["tasks"][1]["input_tokens"].as_u64().unwrap(), 200);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_report_aggregates_are_zero_for_empty_batch() {
        let dir = std::env::temp_dir().join(format!("ta-batch-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.json");
        write_report(&path, &[]).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["totals"]["input_tokens"].as_u64().unwrap(), 0);
        // No div-by-zero: averages fall back to 0.0 for an empty batch.
        assert_eq!(v["averages"]["input_tokens"].as_f64().unwrap(), 0.0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn merge_keeps_prior_successes_and_overlays_fresh() {
        // Prior: alpha failed, beta succeeded. Re-run alpha, now it passes.
        let prior = vec![tr("alpha", false), tr("beta", true)];
        let fresh = vec![tr("alpha", true)];

        let merged = merge_reports(prior, &fresh);

        // Full set present (not just the re-run subset), alpha now green.
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "alpha");
        assert!(merged[0].success);
        assert_eq!(merged[1].name, "beta");
        assert!(merged[1].success); // prior success preserved verbatim
    }

    #[test]
    fn merge_fresh_failure_overrides_prior_success() {
        // A re-run that regresses (was ok, now fails) must record the failure —
        // the fresh result is the truth of record, never stale-optimistic.
        let prior = vec![tr("alpha", true)];
        let fresh = vec![tr("alpha", false)];

        let merged = merge_reports(prior, &fresh);

        assert_eq!(merged.len(), 1);
        assert!(!merged[0].success);
    }

    #[test]
    fn read_prior_tasks_roundtrips_write_report() {
        let dir = std::env::temp_dir().join(format!("ta-batch-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.json");

        // Write a report, then read it back: field names must line up so the
        // merge base is faithful (a mismatch would silently drop entries).
        let wrote = vec![tr("alpha", false), tr("beta", true)];
        write_report(&path, &wrote).unwrap();
        let read = read_prior_tasks(&path).unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].name, "alpha");
        assert!(!read[0].success);
        assert_eq!(read[1].name, "beta");
        assert!(read[1].success);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cancelled_run_marks_all_tasks_cancelled_exit_130() {
        // With the cancel flag already set, the worker pool must gate every
        // task (none run — no provider/agent is built), mark each `cancelled`,
        // still write a report accounting for all tasks, and return 130.
        let dir = std::env::temp_dir().join(format!("ta-batch-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), r#"{"task":"do a"}"#).unwrap();
        std::fs::write(dir.join("b.json"), r#"{"task":"do b"}"#).unwrap();

        let base: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"MOCK_API_KEY","model":"m"}"#,
        )
        .unwrap();
        let report = dir.join("report.json");
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(true)); // pre-cancelled

        let code = run(&base, &dir, 2, &report, 0, false, cancel).unwrap();
        assert_eq!(code, 130);

        // Report accounts for both tasks; both cancelled, none actually run.
        let tasks = read_prior_tasks(&report).unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().all(|t| !t.success));
        assert!(tasks.iter().all(|t| t.stop_reason == "cancelled"));
        assert!(tasks.iter().all(|t| t.turns == 0));
        // No per-task result files were written (tasks never ran).
        let results_dir = dir.join("results");
        assert_eq!(
            std::fs::read_dir(&results_dir).unwrap().count(),
            0,
            "cancelled tasks must not write result files"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn setup_error_is_not_retried() {
        // Agent-construction failures (unset api_key_env here) are
        // deterministic: with --retries 3 the task must fail ONCE as
        // setup_error, not burn three backoffs to fail identically. If it were
        // retried this test would take 1+2+4=7s of backoff; it returns in ms.
        let dir = std::env::temp_dir().join(format!("ta-batch-su-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("t.json"), r#"{"task":"x"}"#).unwrap();
        let base: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"DEFINITELY_UNSET_TA_KEY","model":"m"}"#,
        )
        .unwrap();
        let report = dir.join("report.json");
        let start = std::time::Instant::now();
        let code = run(&base, &dir, 1, &report, 3, false, Default::default()).unwrap();
        assert_eq!(code, 1);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "setup_error must not enter the retry/backoff loop"
        );
        let tasks = read_prior_tasks(&report).unwrap();
        assert_eq!(tasks[0].stop_reason, "setup_error");
        assert!(!tasks[0].success);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cancel_during_retry_backoff_marks_task_cancelled() {
        // L4: a Ctrl-C during a RUNNING task's retry backoff must mark that
        // task cancelled (not run another full attempt) and exit 130. This is
        // distinct from the pre-start gate (cancelled_run_marks_all_tasks_*):
        // here the task already started, errored transiently, and is mid-
        // backoff when cancel arrives. Without the post-sleep check the next
        // attempt would run to completion before the worker's NEXT-task gate
        // caught the cancel.
        //
        // The task's base_url points at a closed local port → the provider
        // request fails with connection refused → agent.run errs → the
        // transient "error" stop_reason, entering the retry loop. (Setup-level
        // failures like an unset api_key_env are "setup_error" — deterministic,
        // NOT retried — so they can't drive this path anymore.)
        // A spawned thread flips cancel ~50ms in, well inside the 1s first
        // backoff, so the post-sleep check trips deterministically (no race:
        // 50ms ≪ 1s). Mirrors single-run's cancel-during-backoff semantics.
        let dir = std::env::temp_dir().join(format!("ta-batch-bo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("t.json"), r#"{"task":"x"}"#).unwrap();

        // Unique env var name so parallel tests can't observe each other's.
        std::env::set_var("TA_BATCH_BO_KEY", "dummy");
        let base: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"TA_BATCH_BO_KEY","model":"m","base_url":"http://127.0.0.1:1"}"#,
        )
        .unwrap();
        let report = dir.join("report.json");
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Flip cancel ~50ms in — inside the first 1s backoff (attempt 0 errors
        // near-instantly: connection refused on a closed port, no network).
        let flag = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            flag.store(true, Ordering::Relaxed);
        });

        let code = run(&base, &dir, 1, &report, 2, false, cancel).unwrap();
        assert_eq!(code, 130, "cancel during backoff → exit 130");

        // The task is marked cancelled (not re-attempted to success/error), with
        // a backoff-cancel message — not the gate's "not started" wording.
        let tasks = read_prior_tasks(&report).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].stop_reason, "cancelled");
        assert!(!tasks[0].success);
        assert!(
            tasks[0]
                .error
                .as_deref()
                .unwrap_or("")
                .contains("backoff"),
            "expected a backoff-cancel message, got {:?}",
            tasks[0].error
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // ─── HTTP-mock helpers for batch integration tests ─────────────────
    //
    // Minimal HTTP/1.1 server on a loopback port. reqwest::blocking talks
    // HTTP/1.1 to it as it would to a real gateway. Each response carries
    // Content-Length AND `connection: close` so reqwest opens a fresh socket
    // per request (no pool reuse). Thread-per-connection so concurrent batch
    // workers are served in parallel — a single accept thread would serialize
    // them and mask the very concurrency these tests exist to prove (the same
    // reason examples/mock_openai.py uses ThreadingHTTPServer).

    /// Read one HTTP request's body off `stream` (parse headers for
    /// Content-Length, then read exactly that many bytes).
    fn read_http_body(stream: &std::net::TcpStream) -> String {
        let mut reader = BufReader::new(stream.try_clone().expect("clone tcp"));
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break; // end of headers
            }
            if let Some(v) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            let _ = reader.read_exact(&mut body);
        }
        String::from_utf8_lossy(&body).into_owned()
    }

    /// Loopback mock that sleeps `delay` on every request then returns a
    /// minimal success SSE, recording each request's [start, end] window.
    /// Thread-per-connection (see note above) so two concurrent workers'
    /// sleep windows actually overlap in wall clock.
    fn slow_ok_mock(delay: Duration) -> (String, Spans) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let spans: Spans = Arc::new(Mutex::new(Vec::new()));
        let spans_c = spans.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let stream = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let spans_c2 = spans_c.clone();
                std::thread::spawn(move || {
                    let mut s = stream;
                    let _body = read_http_body(&s);
                    let start = Instant::now();
                    std::thread::sleep(delay);
                    let end = Instant::now();
                    spans_c2.lock().unwrap().push((start, end));
                    let payload = concat!(
                        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"done\"}}]}\n\n",
                        "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                        "data: [DONE]\n\n",
                    );
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        payload.len(),
                    );
                    let _ = s.write_all(head.as_bytes());
                    let _ = s.write_all(payload.as_bytes());
                    let _ = s.flush();
                });
            }
        });
        (url, spans)
    }

    #[test]
    fn report_json_inside_batch_dir_is_skipped_as_a_task() {
        // Low12: when --report points at a *.json INSIDE the batch dir, the task
        // scanner must skip it (canonicalize compare) — otherwise the prior
        // report.json is parsed as a task fragment and fails (it has report-shape
        // keys; PartialConfig's deny_unknown_fields rejects them). We pre-write
        // report.json at the exact report path plus one real task, cancel-gate
        // the run so no provider is needed, and assert exactly one task was
        // scanned. If the skip were broken, run() would Err at parse time (or a
        // second phantom task would appear) — the count==1 assertion catches it.
        let dir = std::env::temp_dir().join(format!("ta-batch-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let report = dir.join("report.json");
        // A realistic prior report sitting in the batch dir — valid JSON but a
        // REPORT, not a task fragment.
        std::fs::write(
            &report,
            r#"{"total":1,"succeeded":1,"failed":0,"tasks":[{"name":"task-a","success":true}]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("task-a.json"), r#"{"task":"do a"}"#).unwrap();

        let base: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"MOCK_API_KEY","model":"m"}"#,
        )
        .unwrap();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(true)); // gate everything

        let code = run(&base, &dir, 1, &report, 0, false, cancel).unwrap();
        assert_eq!(code, 130);

        let tasks = read_prior_tasks(&report).unwrap();
        // Exactly one task scanned+gated — report.json was NOT treated as a task.
        assert_eq!(
            tasks.len(),
            1,
            "report.json inside the batch dir must be skipped, not scanned as a task"
        );
        assert_eq!(tasks[0].name, "task-a");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jobs_runs_tasks_in_parallel() {
        // G-1: prove --jobs N actually overlaps task execution in wall-clock
        // time. A serial worker pool (or a std::thread::scope that accidentally
        // awaited each task before spawning the next) still passes every
        // functional test — only a timing assertion on a thread-per-connection
        // mock catches the regression. Per the project's own guidance: concurrency
        // needs a threaded-mock e2e, not a functional unit test.
        //
        // Two tasks, jobs=2, each driving one 400ms provider request. The mock
        // records each request's [start, end] sleep window; the rigorous check
        // is that those two windows OVERLAP (serial execution would leave a
        // strict gap: window B starts only after window A ends).
        let delay = Duration::from_millis(400);
        let (url, spans) = slow_ok_mock(delay);
        let dir = std::env::temp_dir().join(format!("ta-batch-par-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), r#"{"task":"do a"}"#).unwrap();
        std::fs::write(dir.join("b.json"), r#"{"task":"do b"}"#).unwrap();

        // Unique env var so parallel tests can't observe each other's state.
        std::env::set_var("TA_BATCH_PAR_KEY", "dummy");
        let base: Config = serde_json::from_str(&format!(
            r#"{{"provider":"openai","api_key_env":"TA_BATCH_PAR_KEY","model":"m","base_url":"{}"}}"#,
            url
        ))
        .unwrap();
        let report = dir.join("report.json");

        let code = run(&base, &dir, 2, &report, 0, false, Default::default()).unwrap();
        assert_eq!(code, 0, "both tasks should succeed against the mock");

        let s = spans.lock().unwrap();
        assert_eq!(s.len(), 2, "exactly two provider requests (one per task)");
        let (a, b) = (s[0], s[1]);
        // Standard interval-overlap test: [a.0,a.1] and [b.0,b.1] overlap iff
        // a.0 <= b.1 && b.0 <= a.1. Serial execution → b.0 > a.1 (a's window
        // closes before b's opens) → the second conjunct fails. With 400ms
        // sleeps and near-simultaneous worker startup, parallel overlap is
        // hundreds of ms — not a boundary case.
        assert!(
            a.0 <= b.1 && b.0 <= a.1,
            "request intervals must overlap for true parallelism: A={a:?} B={b:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
