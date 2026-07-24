use crate::agent::RunOutcome;
use crate::config::{Config, PartialConfig};
use crate::harness;
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex};

struct TaskReport {
    name: String,
    success: bool,
    stop_reason: String,
    turns: u32,
    elapsed_secs: f64,
    error: Option<String>,
}

/// Run every `*.json` task fragment in `dir` against the base config,
/// with a fixed pool of worker threads. Returns process exit code.
pub fn run(base: &Config, dir: &Path, jobs: usize, report_path: &Path) -> Result<i32> {
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
        "batch: {} tasks, {} workers, results → {}",
        tasks.len(),
        jobs,
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
                let report = run_one(base, &name, partial, &results_dir, &client);
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
    let succeeded = reports.iter().filter(|r| r.success).count();
    write_report(report_path, &reports)?;
    eprintln!(
        "batch done: {}/{} succeeded, report → {}",
        succeeded,
        reports.len(),
        report_path.display()
    );
    Ok(if succeeded == reports.len() { 0 } else { 1 })
}

fn run_one(
    base: &Config,
    name: &str,
    partial: PartialConfig,
    results_dir: &Path,
    client: &reqwest::blocking::Client,
) -> TaskReport {
    let start = std::time::Instant::now();
    let fail = |msg: String| TaskReport {
        name: name.into(),
        success: false,
        stop_reason: "error".into(),
        turns: 0,
        elapsed_secs: start.elapsed().as_secs_f64(),
        error: Some(msg),
    };

    let mut cfg = base.clone();
    if let Some(field) = partial.forbidden_batch_override() {
        return fail(format!(
            "task file must not override '{field}' (only task/system_prompt/model/max_tokens/limits \
             are overridable in batch mode)"
        ));
    }
    if let Err(e) = cfg.apply(partial) {
        return fail(format!("{e:#}"));
    }
    if cfg.task.is_none() {
        return fail("task file did not set \"task\" and base config has none".into());
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
                harness::write_result(&results_dir.join(format!("{name}.json")), &o)
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
        Err(e) => fail(format!("{e:#}")),
    }
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
