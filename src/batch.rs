use crate::agent::RunOutcome;
use crate::config::{Config, PartialConfig};
use crate::harness;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex};

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
///
/// Returns process exit code.
pub fn run(
    base: &Config,
    dir: &Path,
    jobs: usize,
    report_path: &Path,
    retries: usize,
    failed: bool,
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
        let kept_successes = prior_tasks.len() - failed_names.len();
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
    // timeout must cover the largest per-task override.
    let max_timeout = tasks
        .iter()
        .filter_map(|(_, p)| p.limits.as_ref()?.timeout_secs)
        .fold(base.limits.timeout_secs, |a, b| a.max(b));
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
                let report = run_one(base, &name, partial, &results_dir, &client, retries);
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
    Ok(if succeeded == final_reports.len() { 0 } else { 1 })
}

/// One attempt at a task. Returns a `stop_reason` that signals retryability:
/// `"error"` is transient (agent.run returned Err — provider/network) and is
/// retried by `run_one`; `"bad_task"` is a deterministic config problem
/// (forbidden override, bad JSON, no task) and is NOT retried; any other value
/// is the model's real stop reason (end_turn / max_turns / …) and is not
/// retried either.
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

    let outcome: Result<(RunOutcome,)> = (|| {
        let task = cfg.task.clone().expect("checked above");
        let initial = vec![crate::provider::Message::user(vec![
            crate::provider::Block::Text(task),
        ])];
        let mut agent = crate::build_agent(&cfg, initial, client)?;
        let o = agent.run(&mut |_| {})?; // quiet: no per-event printing
        Ok((o,))
    })();

    match outcome {
        Ok((o,)) => {
            // A failed result write must not be swallowed: CI reads this file
            // for the full tool_calls/summary. Surface it so report.json does
            // not claim success while the result file is missing.
            let mut error = None;
            if let Err(e) =
                harness::write_result(&results_dir.join(format!("{name}.json")), Some(&o), None)
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
fn run_one(
    base: &Config,
    name: &str,
    partial: PartialConfig,
    results_dir: &Path,
    client: &reqwest::blocking::Client,
    retries: usize,
) -> TaskReport {
    let start = std::time::Instant::now();
    let mut report = run_one_attempt(base, name, &partial, results_dir, client);
    let mut attempt = 0;
    while report.stop_reason == "error" && attempt < retries {
        let backoff = std::time::Duration::from_secs(backoff_secs(attempt));
        eprintln!(
            "[{}] transient error ({}); retry {}/{} in {:?}",
            name,
            report.error.as_deref().unwrap_or(""),
            attempt + 1,
            retries,
            backoff,
        );
        std::thread::sleep(backoff);
        attempt += 1;
        report = run_one_attempt(base, name, &partial, results_dir, client);
    }
    // run_one_attempt stamps per-attempt elapsed; overwrite with the total
    // across attempts + backoffs so report.json reflects real wall cost.
    report.elapsed_secs = start.elapsed().as_secs_f64();
    report
}

/// Exponential backoff (seconds) for retry `attempt` (0-indexed): 1, 2, 4, 8,
/// 16, 32 — the shift is clamped so a large `--retries` can't sleep for minutes
/// (or overflow).
fn backoff_secs(attempt: usize) -> u64 {
    1u64 << attempt.min(5)
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
    fn backoff_grows_then_caps() {
        // 1, 2, 4, 8, 16, 32 — doubling each step…
        assert_eq!(backoff_secs(0), 1);
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(2), 4);
        assert_eq!(backoff_secs(3), 8);
        assert_eq!(backoff_secs(4), 16);
        assert_eq!(backoff_secs(5), 32);
        // …then clamped at 32s so a large --retries never sleeps for minutes
        // (and so the shift can't overflow on absurd inputs).
        assert_eq!(backoff_secs(6), 32);
        assert_eq!(backoff_secs(100), 32);
    }

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
}
