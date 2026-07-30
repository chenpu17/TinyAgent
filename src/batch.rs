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
    let mut files: Vec<PathBuf> = Vec::new();
    for e in std::fs::read_dir(dir)
        .with_context(|| format!("cannot read batch dir: {}", dir.display()))?
    {
        let p = e?.path();
        if p.extension().and_then(|s| s.to_str()) == Some("json") {
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

    // One HTTP client (connection pool) shared by all tasks. The total
    // timeout must cover the largest per-task override — and 0 means
    // "no timeout", which wins over any finite value.
    let max_timeout = tasks
        .iter()
        .filter_map(|(_, p)| p.limits.as_ref()?.timeout_secs)
        .fold(base.limits.timeout_secs, |a, b| {
            if a == 0 || b == 0 { 0 } else { a.max(b) }
        });
    let client = crate::provider::new_client(max_timeout);

    eprintln!(
        "batch: {} tasks, {} workers, {} retries, results → {}",
        tasks.len(),
        jobs,
        retries,
        results_dir.display()
    );

    let (tx, rx) = mpsc::channel::<(String, PartialConfig)>();
    let rx = Mutex::new(rx);
    let reports: Mutex<Vec<TaskReport>> = Mutex::new(Vec::new());

    std::thread::scope(|s| {
        for _ in 0..jobs.max(1) {
            s.spawn(|| loop {
                let next = rx.lock().unwrap().recv();
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
                reports.lock().unwrap().push(report);
            });
        }
        for t in tasks {
            let _ = tx.send(t);
        }
        drop(tx); // workers exit once the queue is drained
    });

    let mut reports = reports.into_inner().unwrap();
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
) -> TaskReport {
    let start = std::time::Instant::now();
    let bad = |stop: &str, msg: String| TaskReport {
        name: name.into(),
        success: false,
        stop_reason: stop.into(),
        turns: 0,
        elapsed_secs: start.elapsed().as_secs_f64(),
        error: Some(msg),
    };

    let mut cfg = base.clone();
    if let Some(field) = partial.forbidden_batch_override() {
        return bad(
            "bad_task",
            format!(
                "task file must not override '{field}' (only task/system_prompt/model/max_tokens/limits \
                 are overridable in batch mode)"
            ),
        );
    }
    if let Err(e) = cfg.apply(partial.clone()) {
        return bad("bad_task", format!("{e:#}"));
    }
    if cfg.task.is_none() {
        return bad(
            "bad_task",
            "task file did not set \"task\" and base config has none".into(),
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
        Err(e) => return bad("setup_error", format!("agent setup failed: {e:#}")),
    };

    let outcome = agent.run(&mut |_| {}); // quiet: no per-event printing

    match outcome {
        Ok(o) => {
            // A failed result write must not be swallowed: CI reads this file
            // for the full tool_calls/summary. Surface it so report.json does
            // not claim success while the result file is missing.
            let mut error = None;
            if let Err(e) =
                harness::write_result(&results_dir.join(format!("{name}.json")), Some(&o), None, 1)
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
            }
        }
        Err(e) => bad("error", format!("{e:#}")),
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
    let mut report = run_one_attempt(base, name, &partial, results_dir, client);
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
        std::thread::sleep(backoff);
        // Honor a Ctrl-C that landed during the backoff: otherwise the next
        // run_one_attempt runs to completion (a full task the user wanted to
        // abort) and only the gate on the NEXT task stops the batch.
        if cancel.load(Ordering::Relaxed) {
            report = TaskReport {
                name: name.into(),
                success: false,
                stop_reason: "cancelled".into(),
                turns: report.turns,
                elapsed_secs: start.elapsed().as_secs_f64(),
                error: Some("cancelled by user during retry backoff".into()),
            };
            break;
        }
        attempt += 1;
        report = run_one_attempt(base, name, &partial, results_dir, client);
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
                "error": r.error,
            })
        })
        .collect();
    let succeeded = reports.iter().filter(|r| r.success).count();
    let report = json!({
        "total": reports.len(),
        "succeeded": succeeded,
        "failed": reports.len() - succeeded,
        "tasks": tasks,
    });
    let bytes = serde_json::to_string_pretty(&report)?;
    harness::atomic_write(path, bytes.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
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
}
