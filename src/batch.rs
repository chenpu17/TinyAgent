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

/// Per-task workdir isolation for `--batch` (the `--isolate` flag).
///
/// `none` (default) keeps today's semantics: all workers share base.workdir.
/// `copy` gives each task a throwaway copy of the workdir; `git` gives each
/// task a `git worktree` of the repo — see `prepare_isolation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IsolateMode {
    #[default]
    None,
    /// Copy the workdir per task (excludes `target`/`node_modules`/`.git`/
    /// `results`). Simple and universal; slow/big for large trees.
    Copy,
    /// `git worktree add --detach` per task. Near-zero copy for repos, but
    /// requires workdir to BE the git repo root (checked up front). Checks
    /// out HEAD: uncommitted/untracked files do NOT enter the worktree —
    /// commit them first or use Copy.
    Git,
}

impl std::str::FromStr for IsolateMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "none" => Ok(IsolateMode::None),
            "copy" => Ok(IsolateMode::Copy),
            "git" => Ok(IsolateMode::Git),
            _ => Err(format!(
                "invalid --isolate value {s:?} (expected none | copy | git)"
            )),
        }
    }
}

/// Directory names skipped by `--isolate copy`. The first three are the size
/// heavyweights (build caches / vendored deps / repo metadata); `results`
/// prevents copying the PREVIOUS batch round's outputs into this task's
/// sandbox. `.tinyagent/` is deliberately NOT excluded — project skills are
/// part of the task's environment.
const COPY_EXCLUDE: [&str; 4] = ["target", "node_modules", ".git", "results"];

/// Prepare a task's isolated workdir. Returns `Ok(None)` for `IsolateMode::None`
/// (no isolation, nothing to clean). The returned guard removes the directory
/// on Drop — including panic unwinds in debug builds. Under release
/// `panic = "abort"` a crashed batch can orphan a temp dir (or a worktree;
/// `git worktree prune` in the source repo recovers those) — accepted.
fn prepare_isolation(
    mode: IsolateMode,
    base: &Config,
    task_name: &str,
) -> Result<Option<IsolationGuard>> {
    if mode == IsolateMode::None {
        return Ok(None);
    }
    let src = match &base.workdir {
        Some(w) => w.clone(),
        None => std::env::current_dir().context("cannot determine current directory")?,
    };
    let tmp = {
        // Same pid + counter trick as harness::atomic_write: unique between
        // concurrent workers and unpredictable against symlink pre-placement.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let safe: String = task_name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        std::env::temp_dir().join(format!("ta-batch-{safe}-{}-{n}", std::process::id()))
    };
    match mode {
        IsolateMode::Copy => {
            std::fs::create_dir_all(&tmp)
                .with_context(|| format!("cannot create isolation dir {}", tmp.display()))?;
            copy_dir_filtered(&src, &tmp)?;
            Ok(Some(IsolationGuard {
                mode,
                dir: tmp,
                src_repo: None,
            }))
        }
        IsolateMode::Git => {
            // The workdir must BE the repo root: `worktree add` checks out the
            // whole repo, so a subdirectory workdir would present a different
            // file set than the task expects — reject rather than silently
            // misplace the task.
            let out = std::process::Command::new("git")
                .args(["-C", &src.display().to_string(), "rev-parse", "--show-toplevel"])
                .output()
                .context("cannot run git (is it on PATH?) for --isolate git")?;
            if !out.status.success() {
                bail!(
                    "--isolate git: workdir {} is not a git repository",
                    src.display()
                );
            }
            let toplevel = String::from_utf8_lossy(&out.stdout).trim().to_string();
            // Compare canonical paths: on macOS temp dirs live under /private
            // while the workdir spelling may be the /var symlink view — a
            // string compare would reject a perfectly good repo root.
            let canon = |p: &str| {
                std::fs::canonicalize(p)
                    .map(|c| c.display().to_string())
                    .unwrap_or_else(|_| p.to_string())
            };
            if canon(&toplevel) != canon(&src.display().to_string()) {
                bail!(
                    "--isolate git: workdir {} is a subdirectory of the repo ({toplevel}); \
                     point workdir at the repo root or use --isolate copy",
                    src.display()
                );
            }
            // Lock contention (index.lock / config.lock held by a concurrent
            // git process on this repo) is the one TRANSIENT failure mode of
            // `worktree add` — and setup_error in batch is otherwise
            // deterministic (not retried by the attempt loop). Bound the
            // retry to lock failures only: a non-lock failure (bad repo,
            // unwritable temp, …) fails fast as before.
            let mut last_err = String::new();
            let mut added = false;
            for attempt in 0..3u32 {
                let out = std::process::Command::new("git")
                    .args([
                        "-C",
                        &toplevel,
                        "worktree",
                        "add",
                        "--detach",
                        &tmp.display().to_string(),
                    ])
                    .output()
                    .context("git worktree add failed to spawn")?;
                if out.status.success() {
                    added = true;
                    break;
                }
                last_err = String::from_utf8_lossy(&out.stderr).to_string();
                if !last_err.contains("lock") {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(
                    200 * u64::from(attempt + 1),
                ));
            }
            if !added {
                bail!("git worktree add failed: {last_err}");
            }
            Ok(Some(IsolationGuard {
                mode,
                dir: tmp,
                src_repo: Some(PathBuf::from(toplevel)),
            }))
        }
        IsolateMode::None => unreachable!("handled above"),
    }
}

/// RAII cleanup for one task's isolated workdir: removes it on Drop (copy:
/// remove_dir_all; git: `worktree remove --force`, falling back to
/// remove_dir_all if git is unavailable/unwilling). Drop keeps debug-build
/// panics from leaking the sandbox.
struct IsolationGuard {
    mode: IsolateMode,
    dir: PathBuf,
    /// Repo the git worktree was carved from (git mode only).
    src_repo: Option<PathBuf>,
}

impl Drop for IsolationGuard {
    fn drop(&mut self) {
        match self.mode {
            IsolateMode::Copy => {
                if let Err(e) = std::fs::remove_dir_all(&self.dir) {
                    eprintln!("[warn] isolation cleanup failed ({}): {e}", self.dir.display());
                }
            }
            IsolateMode::Git => {
                let removed = self.src_repo.as_deref().is_some_and(|repo| {
                    std::process::Command::new("git")
                        .args([
                            "-C",
                            &repo.display().to_string(),
                            "worktree",
                            "remove",
                            "--force",
                            &self.dir.display().to_string(),
                        ])
                        .output()
                        .is_ok_and(|o| o.status.success())
                });
                if !removed {
                    // git missing / index lock / anything: still don't leave the
                    // checkout behind. The worktree's admin metadata is under
                    // <repo>/.git/worktrees; removing the dir without git leaves
                    // a stale entry `git worktree prune` will reap.
                    if let Err(e) = std::fs::remove_dir_all(&self.dir) {
                        eprintln!(
                            "[warn] worktree cleanup failed ({}): {e}; run `git worktree prune` in the repo",
                            self.dir.display()
                        );
                    }
                }
            }
            IsolateMode::None => {}
        }
    }
}

/// Recursively copy `src` into `dst`, skipping TOP-LEVEL entries named in
/// `COPY_EXCLUDE`. The exclusion list means "the previous batch run's
/// artifacts / VCS / build caches at the workdir ROOT" — a nested directory
/// that happens to share a name (`sub/results/`) is task input and must be
/// copied. Relative in-tree symlinks are reproduced (preserves workspace
/// tooling layouts); links resolving outside the source tree are skipped
/// with a warning (see `copy_entry`). Unreadable entries are errors, not
/// skips — a task that silently misses an input file fails confusingly later.
fn copy_dir_filtered(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src)
        .with_context(|| format!("cannot read {}", src.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if COPY_EXCLUDE.contains(&name.as_ref()) {
            continue;
        }
        copy_entry(&entry.path(), &dst.join(entry.file_name()), src, 0)?;
    }
    Ok(())
}

/// Copy one entry (file / dir / symlink) — no exclusion filter: exclusions
/// apply only at the workdir root (see `copy_dir_filtered`).
///
/// Symlink policy — the copy must not contain a road back to the original
/// tree: a relative link that stays inside the source tree is RECREATED
/// verbatim (the common tooling layout, `node_modules/.bin/*`, keeps working
/// and stays copy-local); a link whose target resolves OUTSIDE the tree is
/// SKIPPED with a warning (an absolute link back into the original workdir
/// would re-couple isolated tasks — two workers racing through the same
/// original path — and an external link reaches beyond the copy); an
/// ABSOLUTE link resolving INSIDE the tree is dereferenced (its content
/// copied) because recreated verbatim it would point at the ORIGINAL tree.
/// `depth` caps pathological self-referential dereference chains.
fn copy_entry(from: &Path, to: &Path, root: &Path, depth: usize) -> Result<()> {
    if depth > 64 {
        eprintln!(
            "[warn] isolate copy: skipping {} (dereference chain deeper than 64)",
            from.display()
        );
        return Ok(());
    }
    let ft = std::fs::symlink_metadata(from)
        .with_context(|| format!("cannot stat {}", from.display()))?
        .file_type();
    if ft.is_symlink() {
        let target = std::fs::read_link(from)?;
        // A RELATIVE target resolves against the LINK'S PARENT, not the
        // process CWD.
        let resolved = if target.is_absolute() {
            target.clone()
        } else {
            from.parent().unwrap_or(Path::new(".")).join(&target)
        };
        // Containment on the resolved path. `confined_to` canonicalizes the
        // nearest existing ancestor, so a dangling in-tree link still counts
        // as contained. No symlink privilege on Windows by default: the
        // dereference branch below doubles as the fallback for every
        // contained link there.
        if crate::tools::confined_to(root, &resolved, "isolate copy source").is_err() {
            eprintln!(
                "[warn] isolate copy: skipping symlink {} → {} (escapes the source workdir)",
                from.display(),
                target.display()
            );
            return Ok(());
        }
        if !resolved.exists() {
            // Dangling but contained: recreate the shape (unix) or skip
            // (Windows, which can't create the link) — copying the target is
            // impossible, there is none.
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&target, to).with_context(|| {
                    format!("cannot symlink {} → {}", to.display(), target.display())
                })?;
            }
            #[cfg(not(unix))]
            eprintln!(
                "[warn] isolate copy: skipping dangling symlink {} → {}",
                from.display(),
                target.display()
            );
            return Ok(());
        }
        #[cfg(unix)]
        if target.is_relative() {
            // Relative + contained: recreate verbatim; inside the copy it
            // still points inside the copy.
            std::os::unix::fs::symlink(&target, to).with_context(|| {
                format!("cannot symlink {} → {}", to.display(), target.display())
            })?;
            return Ok(());
        }
        // Absolute + contained + existing: dereference-copy the target.
        if resolved.is_dir() {
            std::fs::create_dir_all(to)?;
            for entry in std::fs::read_dir(&resolved)
                .with_context(|| format!("cannot read {}", resolved.display()))?
            {
                let entry = entry?;
                copy_entry(&entry.path(), &to.join(entry.file_name()), root, depth + 1)?;
            }
        } else {
            std::fs::copy(&resolved, to)?;
        }
    } else if ft.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)
            .with_context(|| format!("cannot read {}", from.display()))?
        {
            let entry = entry?;
            copy_entry(&entry.path(), &to.join(entry.file_name()), root, depth + 1)?;
        }
    } else {
        std::fs::copy(from, to)?;
    }
    Ok(())
}


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
    // Attribution (P3): WHICH model ran this task and WHEN it started. `model`
    // because a task fragment may override it — a mixed-model batch is exactly
    // when per-task attribution matters. `started_at` because parallel workers
    // make elapsed_secs ≠ wall-clock (queue wait + backoff are invisible
    // otherwise). Both `#[serde(default)]` so pre-field reports still parse for
    // `--failed` (same guard as the token fields above). No `provider`: it's a
    // forbidden_batch_override field, so one batch is single-provider — written
    // once at the top level of report.json instead.
    #[serde(default)]
    model: String,
    #[serde(default)]
    started_at: String,
}

/// Run every `*.json` task fragment in `dir` against the base config,
/// with a fixed pool of worker threads.
/// - `retries`: extra attempts per task on transient failures only.
/// - `failed`: re-run only tasks that failed in the prior report at `report_path`.
/// - `isolate`: per-task workdir isolation (`--isolate none|copy|git`).
/// - `cancel`: shared flag flipped by SIGINT (headless). Once set, workers stop
///   dispatching new tasks — in-flight tasks finish on their own (EXCEPT their
///   MCP servers: the shutdown-reaper armed on every agent kills those
///   immediately, the documented 0.3.0 anti-orphan trade-off), not-yet-started
///   ones are marked `cancelled` so the report still accounts for every task.
///   A second Ctrl-C force-exits 130.
///
/// Returns process exit code (130 if cancelled, else 0/1 by success).
#[allow(clippy::too_many_arguments)]
pub fn run(
    base: &Config,
    dir: &Path,
    jobs: usize,
    report_path: &Path,
    retries: usize,
    failed: bool,
    isolate: IsolateMode,
    cancel: Arc<std::sync::atomic::AtomicBool>,
) -> Result<i32> {
    // Reject --jobs 0 explicitly: the worker loop below uses jobs.max(1), so 0
    // would otherwise silently run as a single worker with no warning, which
    // is surprising for a user who passed 0 expecting "no parallelism" or an
    // error. Surface it as a clear config error instead.
    if jobs == 0 {
        bail!("--jobs must be >= 1");
    }
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
            // A DIRECTORY named *.json (a task payload dir, an accidentally
            // nested report dir, …) must not kill the whole batch — the old
            // path fed it to read_to_string, whose "Is a directory" error
            // aborted the run before ANY task started. Skip with a warning;
            // genuinely bad task JSON is still fail-fast below.
            if p.is_dir() {
                eprintln!("[warn] skipping {}: is a directory", p.display());
                continue;
            }
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
    // `--isolate` is the actual fix: each task gets a private workdir (copy or
    // git worktree), removed when the task ends — the warning only fires when
    // the user opted INTO concurrency but NOT into isolation.
    if jobs > 1 && isolate == IsolateMode::None {
        let wd = base
            .workdir
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<current dir>".to_string());
        eprintln!(
            "note: --jobs {jobs} shares one workdir ({wd}); tasks writing the same \
             files can corrupt each other — use --jobs 1, or --isolate copy|git for \
             per-task workdirs."
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
    // Ctrl-C 契约（A5，一次性警告，首个命中 cancel 的 worker 打印）：
    // 未启动的任务跳过并标 cancelled；在跑的任务自身跑完——但它们的
    // MCP server 会被立即杀掉（每个 agent 都用 set_shutdown 挂了同一
    // 个 reaper，防第二次 Ctrl-C 的 _exit(130) 留下孤儿子进程；这是
    // 0.3.0 MCP 孤儿修复的既定取舍，见 run_one_attempt 的 cancel 参数
    // 文档）。
    let warned = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| loop {
                let next = rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                let (name, partial) = match next {
                    Ok(t) => t,
                    Err(_) => return, // channel drained
                };
                let report = if cancel.load(Ordering::Relaxed) {
                    if !warned.swap(true, Ordering::Relaxed) {
                        eprintln!(
                            "[warn] Ctrl-C: not-yet-started tasks are skipped and marked \
                             cancelled; in-flight tasks finish on their own — except their \
                             MCP servers, which are shut down immediately; press Ctrl-C \
                             again to force-exit (130)"
                        );
                    }
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
                        // Never started: no model ran it, no start time. Empty
                        // string = "not applicable", written verbatim.
                        model: String::new(),
                        started_at: String::new(),
                    }
                } else {
                    run_one(base, &name, partial, &results_dir, &client, retries, isolate, &cancel)
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
    write_report(report_path, &final_reports, base.provider.as_str())?;
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
// 9 params — already a cluster of (base, name, partial, results_dir, client)
// plus the accumulator, retry-stable attribution stamp and the shutdown flag;
// bundling into a context struct just moves the same data one indirection away.
#[allow(clippy::too_many_arguments)]
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
    // Process-level Ctrl-C flag. Used ONLY to arm shutdown-aware teardown on
    // the agent — a second Ctrl-C calls process::exit(130), which skips Drop,
    // so without arming the MCP shutdown-reaper here a stuck MCP call (blocked
    // in the 120s CALL_TIMEOUT) would orphan the server child. Deliberately
    // does NOT call set_cancel: the batch worker gate checks this flag before
    // dispatching tasks (CLAUDE.md "批量 Ctrl-C 是 worker gate"), and passing
    // it to set_cancel would race with agent::run()'s entry-point
    // store(false) reset across parallel workers. shutdown is never reset, so
    // it is safe to share (mirrors main.rs single-run, which arms shutdown on
    // the same flag — only the cancel side is batch-specific).
    // CONTRACT (A5): arming on this flag means the FIRST Ctrl-C — which only
    // stops task DISPATCH — also fires the reaper for every in-flight agent's
    // MCP servers (immediate teardown, unlike the "finish on their own" the
    // run() doc promises for the agents themselves). Accepted trade-off from
    // the 0.3.0 MCP-orphan fix: without it, a second Ctrl-C's _exit(130)
    // leaks the servers. The one-shot gate warning tells the user both halves.
    cancel: &Arc<std::sync::atomic::AtomicBool>,
    // RFC 3339 start-of-TASK timestamp (not per-attempt): retries of one task
    // share the task's start, so the bill attributes to when work began.
    started_at: &str,
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
            // bad_task / setup_error fail before apply/build; attribute to the
            // BASE model (a bad fragment's model never ran) and the task start.
            model: base.model.clone(),
            started_at: started_at.to_string(),
        };
    let zero = crate::provider::Usage::default();

    let mut cfg = base.clone();
    if let Some(field) = partial.forbidden_batch_override() {
        return bad(
            "bad_task",
            format!(
                "task file must not override '{field}' (only task/system_prompt/model/max_tokens/\
                 limits/reasoning_effort/inject_environment are overridable in batch mode)"
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
    // Arm MCP shutdown-reaper on the Ctrl-C flag: a second Ctrl-C exits the
    // process (process::exit skips Drop), so without this a task stuck in an
    // MCP call would orphan the server child. See the cancel param doc above.
    agent.set_shutdown(cancel.clone());
    // Attribution for this task's result.json: AFTER apply, so a task fragment
    // overriding `model` is attributed to the model it actually ran.
    let meta = harness::ResultMeta {
        model: agent.model().to_string(),
        provider: agent.provider().to_string(),
        started_at: started_at.to_string(),
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
            if let Err(e) = harness::write_result(
                &results_dir.join(format!("{name}.json")),
                Some(&o),
                None,
                attempt_no,
                "error",
                None,
                &meta,
            ) {
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
                model: meta.model.clone(),
                started_at: meta.started_at.clone(),
            }
        }
        Err(re) => {
            // This attempt failed transiently, but it already burned tokens
            // and ran tools — accumulate them so the next attempt (if any)
            // bills them, and so a final success isn't undercosted.
            acc.add_failed(&re);
            let msg = format!("{re:#}");
            // B4: an unfixable context overflow is deterministic — label it
            // distinctly so run_one's retry gate (stop_reason == "error")
            // skips the backoff ladder, and CI can tell context overflows
            // apart from transient errors. Known corner, accepted: when a
            // TRANSIENT summary failure (compress_failed) stacks on top of a
            // real overflow, a retry might have saved the run — we label it
            // deterministic anyway rather than pay N×(1+2+4…s) of backoff
            // on every genuine overflow.
            let stop = if re.is_context_overflow() {
                "context_overflow"
            } else {
                "error"
            };
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
                stop,
                // `acc` is a &mut param here, so pass it directly (an implicit
                // shared reborrow to &RunCost) — not `&acc` (a needless double
                // borrow). The later `&acc.usage` still borrows fine.
                Some(acc),
                &meta,
            ) {
                eprintln!("[warn] {name}: failed to write result file: {e:#}");
            }
            // Bill the accumulated cost (this attempt + any prior retries) so a
            // failed task reports honest token totals in report.json, not zero.
            let mut r = bad(stop, msg, &acc.usage, acc.turns);
            // Attribute to the model that actually RAN — `bad` stamps the BASE
            // model, which is only correct for bad_task / setup_error (they
            // fail before apply). This run happened after `cfg.apply`, so a
            // fragment's model override must own the bill even on failure.
            r.model = meta.model.clone();
            r
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
#[allow(clippy::too_many_arguments)]
fn run_one(
    base: &Config,
    name: &str,
    partial: PartialConfig,
    results_dir: &Path,
    client: &reqwest::blocking::Client,
    retries: usize,
    isolate: IsolateMode,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
) -> TaskReport {
    let start = std::time::Instant::now();
    // Per-task workdir isolation: prepared ONCE per task (not per attempt) so
    // a retry sees its predecessor's on-disk side effects — matching today's
    // shared-workdir semantics, where attempt 2 runs in the same tree attempt 1
    // mutated. The guard's Drop cleans up when run_one returns.
    let _guard = match prepare_isolation(isolate, base, name) {
        Ok(g) => g,
        Err(e) => {
            // Isolation setup failures are DETERMINISTIC (not a repo, unwritable
            // temp, missing git): report setup_error, no retry, no partial cost.
            return TaskReport {
                name: name.into(),
                success: false,
                stop_reason: "setup_error".into(),
                turns: 0,
                elapsed_secs: start.elapsed().as_secs_f64(),
                error: Some(format!("isolation setup failed: {e:#}")),
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                model: base.model.clone(),
                started_at: crate::harness::now_rfc3339(),
            };
        }
    };
    // With isolation active, tasks run in the sandbox copy — everything
    // downstream (run_one_attempt's clone + apply + agent) sees only that.
    let task_base = match _guard {
        Some(ref g) => {
            let mut b = base.clone();
            b.workdir = Some(g.dir.clone());
            b
        }
        None => base.clone(),
    };
    let base = &task_base;
    // Honest-cost accumulator across attempts (P2-3): a fresh agent is built per
    // attempt, so each failed attempt's tokens/tools can only be recovered via
    // the RunError it returns — accumulate here and fold into the final result.
    let mut acc = RunCost::default();
    // Task-level start (shared by every attempt) for the attribution stamp.
    let started_at = crate::harness::now_rfc3339();
    let mut report =
        run_one_attempt(base, name, &partial, results_dir, client, 1, &mut acc, cancel, &started_at);
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
                // Attribution lost across the attempt boundary: run_one doesn't
                // know the applied model (a fragment may override it). The
                // last attempt's results/<name>.json (written by
                // run_one_attempt with full attribution) is the authoritative
                // record; the report entry just carries what's recoverable.
                model: String::new(),
                started_at: started_at.clone(),
            };
            break;
        }
        attempt += 1;
        report =
            run_one_attempt(base, name, &partial, results_dir, client, attempt + 1, &mut acc, cancel, &started_at);
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

fn write_report(path: &Path, reports: &[TaskReport], provider: &str) -> Result<()> {
    let tasks: Vec<_> = reports
        .iter()
        .map(|r| {
            json!({
                "name": r.name,
                "success": r.success,
                "stop_reason": r.stop_reason,
                "model": r.model,
                "started_at": r.started_at,
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
        // Saturating: matches the core usage folds — absurd per-task numbers
        // must not wrap the batch bill to a small total.
        ti = ti.saturating_add(r.input_tokens);
        to = to.saturating_add(r.output_tokens);
        tcc = tcc.saturating_add(r.cache_creation_input_tokens);
        tcr = tcr.saturating_add(r.cache_read_input_tokens);
    }
    // per-task average; 0 when there are no tasks (avoids div-by-zero).
    let avg = |t: u64| if reports.is_empty() { 0.0 } else { t as f64 / reports.len() as f64 };
    let report = json!({
        "total": reports.len(),
        "succeeded": succeeded,
        "failed": reports.len() - succeeded,
        // One provider per batch (forbidden_batch_override) — attribute the
        // whole report once instead of per task.
        "provider": provider,
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
        write_report(&path, &reports, "openai").unwrap();

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
            model: "test-model".into(),
            started_at: "2026-08-15T07:41:02Z".into(),
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
        write_report(&path, &reports, "openai").unwrap();

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
        // Attribution: top-level provider + per-task model/started_at.
        assert_eq!(v["provider"], "openai");
        assert_eq!(v["tasks"][0]["model"], "test-model");
        assert!(v["tasks"][0]["started_at"].as_str().unwrap().ends_with('Z'));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prior_report_without_attribution_fields_still_parses() {
        // serde(default) guard: a report written before model/started_at
        // existed must still load for --failed (same contract as the token
        // fields when they were added).
        let dir = std::env::temp_dir().join(format!("ta-batch-old-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.json");
        std::fs::write(
            &path,
            r#"{"total":1,"succeeded":1,"failed":0,
                "tasks":[{"name":"a","success":true,"stop_reason":"end_turn",
                "turns":1,"elapsed_secs":1.0,"error":null,
                "input_tokens":10,"output_tokens":5}]}"#,
        )
        .unwrap();
        let prior = read_prior_tasks(&path).unwrap();
        assert_eq!(prior.len(), 1);
        assert!(prior[0].success);
        assert_eq!(prior[0].model, "", "missing model defaults to empty");
        assert_eq!(prior[0].started_at, "", "missing started_at defaults to empty");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_report_aggregates_are_zero_for_empty_batch() {
        let dir = std::env::temp_dir().join(format!("ta-batch-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.json");
        write_report(&path, &[], "openai").unwrap();

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
        write_report(&path, &wrote, "openai").unwrap();
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

        let code = run(&base, &dir, 2, &report, 0, false, IsolateMode::None, cancel).unwrap();
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
        let code = run(&base, &dir, 1, &report, 3, false, IsolateMode::None, Default::default()).unwrap();
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

        let code = run(&base, &dir, 1, &report, 2, false, IsolateMode::None, cancel).unwrap();
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

        let code = run(&base, &dir, 1, &report, 0, false, IsolateMode::None, cancel).unwrap();
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
    fn batch_skips_json_named_directories() {
        // C4: a DIRECTORY named *.json inside the batch dir must be skipped
        // with a warning — feeding it to read_to_string errors "Is a
        // directory" and the old `?` aborted the whole batch before ANY task
        // ran. Cancel-gate the run (no provider needed) and assert exactly
        // the real task was scanned.
        let dir = std::env::temp_dir().join(format!("ta-batch-dirjson-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("decoy.json")).unwrap();
        std::fs::write(dir.join("decoy.json").join("payload.txt"), "x").unwrap();
        std::fs::write(dir.join("task-a.json"), r#"{"task":"do a"}"#).unwrap();

        let base: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"MOCK_API_KEY","model":"m"}"#,
        )
        .unwrap();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(true)); // gate everything

        let report = dir.join("report.json");
        let code = run(&base, &dir, 1, &report, 0, false, IsolateMode::None, cancel).unwrap();
        assert_eq!(
            code, 130,
            "cancel-gated batch: a *.json DIRECTORY must not abort the scan"
        );

        let tasks = read_prior_tasks(&report).unwrap();
        assert_eq!(tasks.len(), 1, "only the real task file is scanned");
        assert_eq!(tasks[0].name, "task-a");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failed_task_attributes_to_fragment_model() {
        // C2: a task fragment overriding `model` must own report.json's
        // attribution even when the run FAILS — the error path used to stamp
        // the BASE model, mis-attributing the bill. Point the provider at a
        // closed port (immediate connection refused → transient Err).
        let dir = std::env::temp_dir().join(format!("ta-batch-modelattr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("t.json"), r#"{"task":"x","model":"override-m"}"#).unwrap();

        std::env::set_var("TA_BATCH_MODELATTR_KEY", "dummy");
        let base: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"TA_BATCH_MODELATTR_KEY","model":"base-m","base_url":"http://127.0.0.1:9"}"#,
        )
        .unwrap();
        let report = dir.join("report.json");

        let code = run(&base, &dir, 1, &report, 0, false, IsolateMode::None, Default::default())
            .unwrap();
        assert_eq!(code, 1, "connection refused → task fails, batch exit 1");

        let tasks = read_prior_tasks(&report).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].model, "override-m",
            "failed run bills the model that actually ran"
        );
        assert_eq!(tasks[0].stop_reason, "error");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Loopback mock that rejects EVERY request with a 400 context-overflow
    /// body (the deterministic too-long rejection).
    fn overflow_400_mock() -> (String, Arc<Mutex<usize>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let count_c = count.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let _body = read_http_body(&stream);
                *count_c.lock().unwrap() += 1;
                let payload = r#"{"error":{"message":"This model's maximum context length is 4096 tokens. However, your messages resulted in 9000 tokens. Please reduce the length of the messages.","type":"invalid_request_error"}}"#;
                let head = format!(
                    "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    payload.len(),
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(payload.as_bytes());
                let _ = stream.flush();
            }
        });
        (url, count)
    }

    #[test]
    fn overflow_failure_is_not_retried_in_batch() {
        // B4: a context-overflow 400 is deterministic (compression already
        // failed to save it — 1-message history has no safe cut). --retries 3
        // must NOT run the backoff ladder: exactly one provider request,
        // stop_reason "context_overflow" (distinct from transient "error"),
        // and the whole task finishes in well under the first backoff second.
        let (url, count) = overflow_400_mock();
        let dir = std::env::temp_dir().join(format!("ta-batch-ovf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("t.json"), r#"{"task":"x"}"#).unwrap();
        std::env::set_var("TA_BATCH_OVF_KEY", "dummy");
        let base: Config = serde_json::from_str(&format!(
            r#"{{"provider":"openai","api_key_env":"TA_BATCH_OVF_KEY","model":"m","base_url":"{}"}}"#,
            url
        ))
        .unwrap();
        let report = dir.join("report.json");

        let start = std::time::Instant::now();
        let code =
            run(&base, &dir, 1, &report, 3, false, IsolateMode::None, Default::default()).unwrap();
        assert_eq!(code, 1);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "no backoff ladder; took {:?}",
            start.elapsed()
        );
        assert_eq!(*count.lock().unwrap(), 1, "exactly one request — no retries");
        let tasks = read_prior_tasks(&report).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].stop_reason, "context_overflow");

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

        let code = run(&base, &dir, 2, &report, 0, false, IsolateMode::None, Default::default()).unwrap();
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

    /// Loopback mock that scripts a real tool-use loop: first response tells
    /// the model to `write_file` (path/body fixed per server), second response
    /// is a plain end_turn. Same thread-per-connection shape as slow_ok_mock.
    /// Per-task state is keyed by hashing the FIRST user message out of the
    /// request body (each task's opening message is unique), so two concurrent
    /// tasks' turns never interleave their counters.
    fn write_file_mock(path: &str, body: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let path = path.to_string();
        let body = body.to_string();
        let seen: Arc<Mutex<std::collections::HashMap<u64, usize>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(s) = stream else { continue };
                let (path, body, seen) = (path.clone(), body.clone(), seen.clone());
                std::thread::spawn(move || {
                    let mut s = s;
                    let req = read_http_body(&s);
                    // Key = hash of the first user message content ("do a"/"do b").
                    // Content arrives as an OpenAI-style block array
                    // [{"type":"text","text":"do a"}], not a bare string.
                    let key: u64 = {
                        let v: serde_json::Value = serde_json::from_str(&req).unwrap_or_default();
                        let first_user_text = v["messages"].as_array().and_then(|msgs| {
                            msgs.iter().find(|m| m["role"] == "user").and_then(|m| {
                                m["content"].as_str().map(String::from).or_else(|| {
                                    m["content"].as_array().and_then(|blocks| {
                                        blocks
                                            .iter()
                                            .find(|b| b["type"] == "text")
                                            .and_then(|b| b["text"].as_str())
                                            .map(String::from)
                                    })
                                })
                            })
                        });
                        match first_user_text {
                            Some(t) => t.bytes().fold(0xcbf29ce484222325, |a, b| {
                                (a ^ b as u64).wrapping_mul(0x100000001b3)
                            }),
                            None => 0,
                        }
                    };
                    let turn = {
                        let mut g = seen.lock().unwrap();
                        let n = g.entry(key).or_insert(0);
                        *n += 1;
                        *n
                    };
                    let payload = if turn == 1 {
                        // Build the SSE payload with serde_json (string
                        // surgery here is a nest of JSON-in-JSON escapes; a
                        // hand-built template got the closing quotes wrong).
                        let args = json!({"path": path, "content": body})
                            .to_string();
                        let line1 = json!({
                            "choices": [{"index": 0, "delta": {"role": "assistant",
                                "tool_calls": [{"index": 0, "id": "c1", "type": "function",
                                    "function": {"name": "write_file", "arguments": args}}]}}]
                        })
                        .to_string();
                        let line2 = json!({
                            "choices": [{"index": 0, "finish_reason": "tool_calls", "delta": {}}]
                        })
                        .to_string();
                        format!("data: {line1}\n\ndata: {line2}\n\ndata: [DONE]\n\n")
                    } else {
                        let line1 = json!({
                            "choices": [{"index": 0, "delta": {"role": "assistant", "content": "done"}}]
                        })
                        .to_string();
                        let line2 = json!({
                            "choices": [{"index": 0, "finish_reason": "stop", "delta": {}}]
                        })
                        .to_string();
                        format!("data: {line1}\n\ndata: {line2}\n\ndata: [DONE]\n\n")
                    };
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
        url
    }

    #[test]
    fn isolate_mode_parses() {
        use std::str::FromStr;
        assert_eq!(IsolateMode::from_str("none").unwrap(), IsolateMode::None);
        assert_eq!(IsolateMode::from_str("copy").unwrap(), IsolateMode::Copy);
        assert_eq!(IsolateMode::from_str("git").unwrap(), IsolateMode::Git);
        assert_eq!(IsolateMode::from_str("COPY").unwrap(), IsolateMode::Copy);
        let err = IsolateMode::from_str("banana").unwrap_err();
        assert!(err.contains("none | copy | git"), "got: {err}");
    }

    #[test]
    fn isolate_copy_gives_each_task_a_private_workdir() {
        // THE isolation contract: two tasks write the SAME file concurrently;
        // without isolation they corrupt each other (or one silently wins),
        // with --isolate copy both succeed and the SOURCE workdir is untouched.
        // Also proves the temp dirs are cleaned up (no ta-batch-* litter).
        let url = write_file_mock("out.txt", "task wrote\n");
        let root = std::env::temp_dir().join(format!("ta-batch-iso-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workdir = root.join("src");
        std::fs::create_dir_all(&workdir).unwrap();
        std::fs::write(workdir.join("seed.txt"), "seed").unwrap();
        let dir = root.join("tasks");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), r#"{"task":"do a"}"#).unwrap();
        std::fs::write(dir.join("b.json"), r#"{"task":"do b"}"#).unwrap();

        std::env::set_var("TA_BATCH_ISO_KEY", "dummy");
        let base: Config = serde_json::from_str(&format!(
            r#"{{"provider":"openai","api_key_env":"TA_BATCH_ISO_KEY","model":"m","base_url":"{}","workdir":"{}","tools":["write_file"]}}"#,
            url,
            workdir.display()
        ))
        .unwrap();
        let report = dir.join("report.json");

        let code = run(&base, &dir, 2, &report, 0, false, IsolateMode::Copy, Default::default())
            .unwrap();
        assert_eq!(code, 0, "both isolated tasks succeed writing the same file");

        // The source workdir keeps its seed and gains nothing: task outputs
        // lived and died in the sandboxes.
        assert!(workdir.join("seed.txt").exists());
        assert!(
            !workdir.join("out.txt").exists(),
            "isolated write must not leak into the source workdir"
        );
        // Sandbox cleanup: no ta-batch-* temp dirs left behind.
        let leftover = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("ta-batch-a-{}", std::process::id()))
            })
            .count();
        assert_eq!(leftover, 0, "isolation temp dirs must be cleaned up");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn isolate_copy_excludes_heavy_directories() {
        // The exclusion list is load-bearing: copying the previous round's
        // results/ or a multi-GB target/ would be both slow and wrong (stale
        // outputs presented as task inputs). It applies ONLY at the workdir
        // root — a nested directory sharing a name (`sub/results/`) is task
        // input and must be copied.
        let root = std::env::temp_dir().join(format!("ta-batch-exc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        for d in ["target", "node_modules", ".git", "results"] {
            std::fs::create_dir_all(src.join(d)).unwrap();
            std::fs::write(src.join(d).join("heavy.bin"), "x").unwrap();
        }
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("keep.txt"), "x").unwrap();
        std::fs::write(src.join("sub").join("inner.txt"), "x").unwrap();
        // A nested `results` directory is task input, not last round's output.
        std::fs::create_dir_all(src.join("sub").join("results")).unwrap();
        std::fs::write(src.join("sub").join("results").join("deep.csv"), "x").unwrap();

        let base: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","workdir":"SRC"}"#,
        )
        .unwrap();
        let base = {
            let mut b = base;
            b.workdir = Some(src.clone());
            b
        };
        let guard = prepare_isolation(IsolateMode::Copy, &base, "exc").unwrap().unwrap();
        let iso = guard.dir.clone();
        for d in ["target", "node_modules", ".git", "results"] {
            assert!(!iso.join(d).exists(), "{d} must be excluded from the copy");
        }
        assert!(iso.join("keep.txt").exists());
        assert!(iso.join("sub").join("inner.txt").exists(), "nested files copied");
        assert!(
            iso.join("sub").join("results").join("deep.csv").exists(),
            "nested results/ is task input — must be copied (root-level filter only)"
        );
        drop(guard);
        assert!(!iso.exists(), "guard Drop removes the sandbox");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    #[cfg(unix)]
    fn isolate_copy_symlink_policy_keeps_the_copy_self_contained() {
        // The copy must not contain a road back to the original tree (an
        // absolute link into the source would re-couple parallel tasks) or
        // out of it. Relative in-tree links are recreated (tooling layouts
        // like node_modules/.bin keep working, copy-local); absolute links
        // resolving inside the tree are dereferenced (content copied);
        // anything resolving outside — and dangling links on Windows — is
        // skipped with a warning instead of reproduced.
        let root = std::env::temp_dir().join(format!("ta-batch-sym-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        let outside = root.join("outside-cache");
        std::fs::create_dir_all(src.join("real")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(src.join("real").join("data.txt"), "d").unwrap();
        std::fs::write(outside.join("leak.txt"), "l").unwrap();
        // 1. Relative in-tree: `data → ./real` — recreated verbatim.
        std::os::unix::fs::symlink("real", src.join("data")).unwrap();
        // 2. Absolute INSIDE the tree: dereferenced (content copied), because
        // recreated verbatim it would point at the ORIGINAL tree.
        std::os::unix::fs::symlink(src.join("real"), src.join("abs-in")).unwrap();
        // 3. Escape: absolute link to a dir outside the source — skipped.
        std::os::unix::fs::symlink(&outside, src.join("escape")).unwrap();
        // 4. Relative escape (`../outside-cache`) — skipped.
        std::os::unix::fs::symlink("../outside-cache", src.join("rel-escape")).unwrap();

        let base: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","workdir":"SRC"}"#,
        )
        .unwrap();
        let base = {
            let mut b = base;
            b.workdir = Some(src.clone());
            b
        };
        let guard = prepare_isolation(IsolateMode::Copy, &base, "sym").unwrap().unwrap();
        let iso = guard.dir.clone();
        // 1 — recreated as a symlink, still relative.
        assert!(
            std::fs::symlink_metadata(iso.join("data"))
                .expect("relative in-tree link copied")
                .file_type()
                .is_symlink(),
            "relative in-tree links are recreated verbatim"
        );
        // 2 — a real directory (deref), not a link back at the original.
        assert!(
            !std::fs::symlink_metadata(iso.join("abs-in"))
                .expect("absolute in-tree link dereferenced")
                .file_type()
                .is_symlink()
                && iso.join("abs-in").join("data.txt").exists(),
            "absolute in-tree links are content-copied"
        );
        // 3/4 — gone entirely: no road out of (or back into) the copy.
        assert!(!iso.join("escape").exists(), "escaping symlink must be skipped");
        assert!(!iso.join("rel-escape").exists(), "relative escape must be skipped");
        drop(guard);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn isolate_git_rejects_non_repo_workdir_as_setup_error() {
        // git mode requires workdir to BE a repo root; anything else is a
        // deterministic setup_error (no retry — elapsed stays sub-second).
        let root = std::env::temp_dir().join(format!("ta-batch-gitbad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workdir = root.join("plain");
        std::fs::create_dir_all(&workdir).unwrap();
        let dir = root.join("tasks");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("t.json"), r#"{"task":"x"}"#).unwrap();

        let base: Config = serde_json::from_str(&format!(
            r#"{{"provider":"openai","api_key_env":"K","model":"m","workdir":"{}"}}"#,
            workdir.display()
        ))
        .unwrap();
        let report = dir.join("report.json");

        let start = std::time::Instant::now();
        let code = run(&base, &dir, 1, &report, 3, false, IsolateMode::Git, Default::default())
            .unwrap();
        assert_eq!(code, 1);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "setup_error must not be retried"
        );
        let tasks = read_prior_tasks(&report).unwrap();
        assert_eq!(tasks[0].stop_reason, "setup_error");
        assert!(
            tasks[0].error.as_deref().unwrap_or("").contains("isolation setup failed"),
            "got: {:?}",
            tasks[0].error
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn isolate_git_gives_each_task_a_worktree_and_keeps_repo_clean() {
        // A real git repo: tasks write into their worktree sandboxes; the base
        // repo stays pristine (`git status --porcelain` empty) and no worktree
        // admin entries linger afterwards.
        let url = write_file_mock("out.txt", "task wrote\n");
        let root = std::env::temp_dir().join(format!("ta-batch-gitiso-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("git on PATH")
        };
        assert!(git(&["init", "-q"]).status.success());
        std::fs::write(repo.join("seed.txt"), "seed").unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-qm", "init"]).status.success());

        let dir = root.join("tasks");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), r#"{"task":"do a"}"#).unwrap();
        std::fs::write(dir.join("b.json"), r#"{"task":"do b"}"#).unwrap();

        std::env::set_var("TA_BATCH_GITISO_KEY", "dummy");
        let base: Config = serde_json::from_str(&format!(
            r#"{{"provider":"openai","api_key_env":"TA_BATCH_GITISO_KEY","model":"m","base_url":"{}","workdir":"{}","tools":["write_file"]}}"#,
            url,
            repo.display()
        ))
        .unwrap();
        let report = dir.join("report.json");

        let code = run(&base, &dir, 2, &report, 0, false, IsolateMode::Git, Default::default())
            .unwrap();
        assert_eq!(code, 0, "both worktree tasks succeed");

        // Base repo pristine: no tracked changes, no untracked out.txt.
        let status = git(&["status", "--porcelain"]);
        assert_eq!(
            String::from_utf8_lossy(&status.stdout).trim(),
            "",
            "base repo must be untouched: {}",
            String::from_utf8_lossy(&status.stdout)
        );
        assert!(!repo.join("out.txt").exists(), "writes stayed in the worktrees");
        // Worktree admin cleaned: `git worktree list` shows only the main one.
        let list = git(&["worktree", "list"]);
        let out = String::from_utf8_lossy(&list.stdout);
        assert_eq!(
            out.lines().count(),
            1,
            "no lingering worktrees after cleanup: {out}"
        );

        std::fs::remove_dir_all(&root).ok();
    }
}
