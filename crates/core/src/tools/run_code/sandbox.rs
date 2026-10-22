//! wasmtime sandbox setup: the engine/module/linker process singletons,
//! per-execution `HostCtx`, the linear-memory limiter, and the wall clock.

use super::*;

/// The sandbox guest: quickjs-ng + glue (see module doc). Embedded so the
/// binary is self-contained — no filesystem dependency at run time.
pub(super) const QUICKJS_WASM: &[u8] = include_bytes!("../../../assets/quickjs.wasm");

/// The linked module starts at 32 MiB of linear memory (`--initial-memory`),
/// so a lower `mem_mb` cannot be honored — the floor is the module itself.
pub(super) const MODULE_INITIAL_MEMORY: usize = 33_554_432;
/// Table-element ceiling for the memory limiter (see `table_growing`) —
/// generous vs. any legitimate funcref/ref table, far below host-risk scale.
pub(super) const MAX_TABLE_ELEMENTS: usize = 1 << 20;

// ── process-level wasm singletons ──────────────────────────────────────────
//
// Init results are cached inside the OnceLock (`OnceLock<Result<T, String>>`)
// because `get_or_try_init` is not stable: a failed init is remembered, not
// retried per call — these are "cannot happen" paths (embedded asset) except
// the registry, whose failure (bad cfg.tools) is a config error best reported
// identically on every call.

pub(super) fn engine() -> Result<&'static Engine> {
    static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let mut cfg = wasmtime::Config::new();
            cfg.consume_fuel(true);
            // Epoch interruption (§6): lets the epoch callback below fire
            // even inside a tight wasm loop — the delivery vehicle for cancel
            // and the wall clock (fuel/mem have their own native signals).
            cfg.epoch_interruption(true);
            // Recursion bound: quickjs-ng disables its own stack limit under
            // __wasi__ (update_stack_limit hard-returns "no limit"), so deep
            // guest recursion must bottom out on THIS cap as a trap we
            // attribute via Trap::StackOverflow. The size is picked to make
            // that cap WIN the race against quickjs's own unchecked internal
            // per-recursion state in linear memory (which at 1.5 MiB
            // corrupted first and surfaced as an unattributable OOB trap —
            // 2524 frames ≈ 1.45 MiB, measured). 384 KiB ≈ 600 JS frames and
            // still sits far under the 2 MiB default stack of the scoped
            // threads the parallel tool fan-out runs on.
            cfg.max_wasm_stack(384 * 1024);
            // Disk compilation cache (v2 P1): quickjs.wasm's machine code is
            // cached on disk keyed by the module's content hash, so a fresh
            // process stops re-paying the lazy compile (debug ≈10s — part of
            // the code arm's wall-time cost the A/B measured). EXPLICIT
            // directory via our own handwritten XDG resolution (mirrors
            // skills::default_global_skills_dir) rather than
            // CacheConfig::load_default: the default's behavior under musl /
            // no-HOME embedded environments is not ours to control. Any
            // failure here degrades to "no cache" — the sandbox must never
            // refuse to start over caching.
            if let Some(dir) = wasmtime_cache_dir() {
                let mut cc = wasmtime::CacheConfig::new();
                cc.with_directory(&dir); // must be absolute — both base branches guarantee it
                if let Ok(cache) = wasmtime::Cache::new(cc) {
                    cfg.cache(Some(cache));
                }
            }
            Engine::new(&cfg).map_err(|e| format!("wasmtime engine init: {e}"))
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// $XDG_CACHE_HOME (when absolute) else $HOME/.cache, then
/// `tinyagent/wasmtime`. `None` when neither env var is set — the caller
/// treats that as "no cache" (degraded, not an error; an embedded host with
/// no HOME still gets a working sandbox).
pub(super) fn wasmtime_cache_dir() -> Option<std::path::PathBuf> {
    wasmtime_cache_dir_under(
        std::env::var_os("XDG_CACHE_HOME").map(std::path::PathBuf::from),
        std::env::var_os("HOME").map(std::path::PathBuf::from),
    )
}

/// Param-injectable core of [`wasmtime_cache_dir`] (env reads are process-
/// global and tests run in parallel); still performs the mkdir, which is the
/// operation whose failure decides the outcome.
pub(super) fn wasmtime_cache_dir_under(
    xdg: Option<std::path::PathBuf>,
    home: Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    let base = xdg
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(".cache")).filter(|p| p.is_absolute()))?;
    let dir = base.join("tinyagent").join("wasmtime");
    std::fs::create_dir_all(&dir).ok().map(|_| dir)
}

/// Start the global epoch ticker (§6): a daemon thread nudging the engine's
/// epoch every ~10ms, so a store whose deadline callback returns `Renew`
/// gets re-checked even while the guest spins in a tight loop. Lazily
/// started on the first run_code execution; lives until process exit (same
/// liveness class as the MCP reaper — one sleeping thread, no cleanup path).
pub(super) fn ensure_ticker() {
    static TICKER: OnceLock<()> = OnceLock::new();
    TICKER.get_or_init(|| {
        // eval's module() warm-up runs BEFORE this and returns a graceful
        // ToolOutput error on engine failure — by the time we get here the
        // engine is provably up. Still, don't panic if that ordering ever
        // changes (arch-review #3): skip arming the ticker and let eval's
        // own engine() call surface the error; without an engine there is
        // no guest execution to deliver interrupts into anyway.
        let Ok(engine) = engine() else { return };
        let engine = engine.clone();
        // Same degradation class as the engine arm above: thread CREATION
        // failure is resource exhaustion, and under release panic="abort" an
        // .expect here took down the whole process — the exact inconsistency
        // the graceful engine path exists to avoid (external re-review). Skip
        // arming with a stderr note instead: fuel/mem still bound the guest,
        // only the epoch (wall-clock) interruption is lost.
        if std::thread::Builder::new()
            .name("code-epoch-ticker".into())
            .spawn(move || loop {
                engine.increment_epoch();
                std::thread::sleep(std::time::Duration::from_millis(10));
            })
            .is_err()
        {
            crate::diag::diag_warn!("run_code: epoch-ticker thread failed to spawn — wall-clock interruption disabled (fuel/mem limits still active)");
        }
    });
}

pub(super) fn module() -> Result<&'static Module> {
    static MODULE: OnceLock<Result<Module, String>> = OnceLock::new();
    MODULE
        .get_or_init(|| {
            let build = || -> Result<Module> {
                Module::new(engine()?, QUICKJS_WASM).context("compile quickjs.wasm")
            };
            build().map_err(|e| format!("{e:#}"))
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

pub(super) fn linker() -> Result<&'static Linker<HostCtx>> {
    static LINKER: OnceLock<Result<Linker<HostCtx>, String>> = OnceLock::new();
    LINKER
        .get_or_init(|| {
            let build = || -> Result<Linker<HostCtx>> {
                let mut l = Linker::new(engine()?);
                // Closures are stateless and read all state through
                // `Caller::data()`, so one Linker serves every execution.
                l.func_wrap(
                    "env",
                    "host_log",
                    |mut caller: Caller<'_, HostCtx>, ptr: i32, len: i32| {
                        // Log lines share the 1 MiB read cap; any read
                        // failure (size or pointer class) drops the line —
                        // logs are best-effort by contract.
                        let Ok(line) = read_guest(caller.get_export("memory"), &caller, ptr, len)
                        else {
                            return;
                        };
                        // Log cap (§6 中-3, pulled forward from M3): the
                        // host-side buffer is bounded at 2× max_result_chars
                        // — an unbounded `while(1) console.log(s)` loop must
                        // cost fuel, not host memory. Past the cap collection
                        // stops, the dropped counter moves, and the CodeLog
                        // events stop with it (no event flood past the cap).
                        let d = caller.data_mut();
                        // +1 per line = the join's '\n' AND a minimum cost
                        // per line: a bare `console.log()` line is 0 chars
                        // and would sail past a chars-only check forever
                        // (sec-review M-1 — `while (1) console.log()` built
                        // an unbounded Vec AND an unbounded CodeLog event
                        // stream on default fuel).
                        let cost = line.chars().count() + 1;
                        // saturating per the numeric-writer convention — a
                        // config-driven max_result_chars near usize::MAX must
                        // disable the cap, not wrap past it.
                        if d.logs_chars.saturating_add(cost)
                            > d.snap.max_result_chars.saturating_mul(2)
                        {
                            d.logs_dropped += 1;
                            return;
                        }
                        d.logs_chars += cost;
                        d.logs.push(line.clone());
                        // Real-time feed (§8.2 D8): a plain `CodeLog` event per
                        // collected line. SAFETY: same window as
                        // host_call_tool's dispatch — imports run synchronously
                        // inside the eval_code call made from run_with_emit's
                        // frame; the pointer never aliases HostCtx itself.
                        let emit = unsafe { &mut *d.emit };
                        emit(AgentEvent::CodeLog(line));
                    },
                )?;
                l.func_wrap(
                    "env",
                    "host_call_tool",
                    |mut caller: Caller<'_, HostCtx>,
                     name_ptr: i32,
                     name_len: i32,
                     args_ptr: i32,
                     args_len: i32|
                     -> i64 {
                        host_call_tool(&mut caller, name_ptr, name_len, args_ptr, args_len)
                    },
                )?;
                Ok(l)
            };
            build().map_err(|e| format!("{e:#}"))
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

// ── per-execution state ────────────────────────────────────────────────────

/// Linear-memory cap (§6). Denial is recorded so a trapped eval can be
/// attributed to `mem` rather than fuel — wasmtime traps carry no source.
pub(super) struct MemLimiter {
    pub(super) max_bytes: usize,
    pub(super) exceeded: bool,
}

impl ResourceLimiter for MemLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, anyhow::Error> {
        if desired > self.max_bytes {
            self.exceeded = true;
            return Ok(false);
        }
        Ok(true)
    }
    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, anyhow::Error> {
        // Tables are host-side memory OUTSIDE the linear-memory cap, so an
        // unguarded grow would bypass mem_mb (sec-review n-2). Unreachable
        // with the current asset (C-compiled, no `table.grow`), but the
        // limiter is the designated resource boundary for asset swaps too.
        Ok(desired <= MAX_TABLE_ELEMENTS)
    }
}

/// One inner tool call's audit-trace entry (§8.4). `in_hash` is the GUEST's
/// original input (the audit-chain view — what the code asked for, aligned
/// with `ToolCallRecord` keeping original inputs); `in_hash_eff` is the
/// post-`Modify` input that actually executed (what happened) — present only
/// when a `Modify` changed it, so the common case stores no duplicate.
pub(super) struct CallTrace {
    pub(super) name: String,
    pub(super) ok: bool,
    pub(super) ms: u128,
    pub(super) in_len: usize,
    pub(super) in_hash: String,
    pub(super) in_hash_eff: Option<String>,
}

/// Everything the imports need, reachable via `Store::data()`. Held in `Arc`s
/// so the struct is `'static` (Store data must be) while sharing the registry.
pub(super) struct HostCtx {
    /// Event sink aliased from `run_with_emit`'s `emit` borrow via a lifetime-
    /// erased fat pointer (see the transmute there for the soundness window).
    pub(super) emit: *mut (dyn FnMut(AgentEvent) + 'static),
    pub(super) tools: Arc<Vec<Box<dyn Tool>>>,
    pub(super) defs: Arc<Vec<ToolDef>>,
    pub(super) ctx: ToolCtx,
    pub(super) logs: Vec<String>,
    /// Chars collected into `logs` so far, and lines dropped past the 2×
    /// max_result_chars cap (§6 中-3) — together they bound host memory a
    /// log-flooding guest can cost.
    pub(super) logs_chars: usize,
    pub(super) logs_dropped: u64,
    pub(super) limiter: MemLimiter,
    /// Usage billed by inner tool calls (`task`-style ToolOutput::usage),
    /// folded into run_code's own output so the outer loop counts it once.
    pub(super) usage: Usage,
    /// The execution's config snapshot (deny lists, call limit, …).
    pub(super) snap: CodeConfig,
    /// Canonicalized workdir — the base the interception layer relativizes
    /// guest paths against (§7). Computed once per execution.
    pub(super) workdir_canon: PathBuf,
    /// Audit trace of every inner call (§8.4) — lands in `ToolOutput.meta`.
    pub(super) calls: Vec<CallTrace>,
    /// Once set (call-limit breach / wall-clock exhaustion), every subsequent
    /// host_call_tool — including `__tools_list` — fails fast with this
    /// reason: an over-limit guest must not keep burning host time even in
    /// "cheap" calls (design §5.2 低-1 puts the entry check first). Carries
    /// the structured-error `kind` ("budget" / "deadline") alongside the
    /// message so the re-thrown Error keeps its class (v2 review).
    pub(super) halted: Option<HaltState>,
    /// Hard wall clock (§6 execution point ①): start + min(code.timeout_secs,
    /// ctx.deadline remaining); `None` = no wall clock (both sources absent —
    /// cancel/fuel/mem still bound the run, the documented boundary). Carries
    /// which side bound it so the termination message names the right knob.
    pub(super) deadline: Option<WallDeadline>,
    /// Set when cancel or the wall clock fires (§6 four-source flags — these
    /// two arrive via the epoch callback / entry check; fuel and mem have
    /// their own native signals). `terminated` maps it to a source-specific
    /// message.
    pub(super) interrupt: Option<Interrupt>,
    /// Session registration set for the lib family (MD-14): `(workdir-relative
    /// canonical path, sha256 of delivered bytes)` pairs the session has
    /// registered via `__hash_file` (i.e. via `saveLib`). Clone-shared across
    /// every execution of one tool lineage, never persisted — a fresh
    /// Agent/process starts empty, `--resume` included ("clone 共享、不跨
    /// 会话恢复"). This is the ONLY admission input the guest cannot forge:
    /// the host reads and hashes the bytes itself.
    pub(super) lib_sessions: LibSessions,
    /// LD-12 ledger for the sandbox `llm` primitive: per-model known usage,
    /// unknown-call counts, reservation totals and over_reserve events,
    /// folded from each call's meta marker by the completion tails. Emitted
    /// into `ToolOutput.meta["llm"]` only when calls > 0 (additive-key
    /// discipline — llm-off runs stay byte-identical).
    pub(super) llm: LlmAudit,
}

/// Which asynchronous limit cut the guest short (§6). Cancel and the wall
/// clock share the epoch-delivery path but stay distinguishable for the
/// error message and the audit story.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Interrupt {
    Cancel,
    Deadline,
}

impl HostCtx {
    pub(super) fn new(
        emit: *mut dyn FnMut(AgentEvent),
        registry: &Registry,
        ctx: ToolCtx,
        max_bytes: usize,
        snap: CodeConfig,
        deadline: Option<WallDeadline>,
        lib_sessions: LibSessions,
    ) -> Self {
        let workdir_canon = ctx
            .workdir
            .canonicalize()
            .unwrap_or_else(|_| ctx.workdir.clone());
        Self {
            emit,
            tools: Arc::clone(&registry.tools),
            defs: Arc::clone(&registry.defs),
            ctx,
            logs: Vec::new(),
            logs_chars: 0,
            logs_dropped: 0,
            limiter: MemLimiter {
                max_bytes,
                exceeded: false,
            },
            usage: Usage::default(),
            snap,
            workdir_canon,
            calls: Vec::new(),
            halted: None,
            deadline,
            interrupt: None,
            lib_sessions,
            llm: LlmAudit::default(),
        }
    }
}

/// LD-12 audit ledger for the sandbox `llm` primitive (see `HostCtx::llm`).
/// "unknown" is a first-class state, not zero: a call whose usage never
/// arrived keeps its reservation standing and counts under `unknown_calls`
/// — the totals are KNOWN usage, never presented as the complete bill.
#[derive(Default)]
pub(super) struct LlmAudit {
    pub(super) calls: u64,
    pub(super) reserved: u64,
    pub(super) settled: u64,
    pub(super) over_reserve: u64,
    /// Per model (BTreeMap: deterministic key order in the JSON audit).
    pub(super) models: std::collections::BTreeMap<String, LlmModelSpend>,
}

#[derive(Default)]
pub(super) struct LlmModelSpend {
    pub(super) known_usage: u64,
    pub(super) known_calls: u64,
    pub(super) unknown_calls: u64,
}

/// A latched halt (call limit / wall clock): the guest-visible message plus
/// the structured-error class for every subsequent failed call.
#[derive(Clone)]
pub(super) struct HaltState {
    pub(super) kind: &'static str,
    pub(super) why: String,
}

/// §6 execution point ①: the hard wall clock = start + min(code.timeout_secs,
/// ctx.deadline remaining). `None` when BOTH sources are absent — no wall
/// clock, the documented boundary (cancel/fuel/mem still bound the run). An
/// already-past ctx deadline clamps to "now" (fires on the first epoch tick).
/// The deadline interrupt's user-facing reason — names the knob that bound
/// the run (cfg timeout, the parent run's remaining time, or both).
pub(super) fn deadline_why(snap: &CodeConfig, bound_by_cfg: bool) -> String {
    if bound_by_cfg {
        format!(
            "wall clock exhausted (code.timeout_secs={} — raise it or simplify the code)",
            snap.timeout_secs
        )
    } else if snap.timeout_secs > 0 {
        "wall clock exhausted (bound by the parent run's remaining time — \
         code.timeout_secs is higher, raising it won't help)"
            .to_string()
    } else {
        "wall clock exhausted (the parent run's deadline — raise it or \
         simplify the code)"
            .to_string()
    }
}

/// The armed wall clock: the instant AND whether the cfg knob was the side
/// that bound it (`deadline_why` names the knob the model can actually turn
/// — correctness-review m-1: when the parent run's remaining time is the
/// tighter side, pointing at `code.timeout_secs` sends it chasing the wrong
/// knob).
#[derive(Clone, Copy)]
pub(super) struct WallDeadline {
    pub(super) at: Instant,
    pub(super) from_cfg: bool,
}

pub(super) fn wall_deadline(snap: &CodeConfig, ctx: &ToolCtx) -> Option<WallDeadline> {
    let now = Instant::now();
    // checked_add mirrors agent.rs's limits.timeout_secs discipline (and its
    // huge_timeout_secs test): a `timeout_secs` near u64::MAX overflows the
    // clock and must degrade to "no wall clock" (fuel/cancel/mem still bound
    // the run), not panic — release builds abort the whole process
    // (correctness-review M-2; untrusted batch fragments can reach this via
    // TightenOrInit adopting an arbitrary finite value over a 0 base).
    let from_cfg = (snap.timeout_secs > 0)
        .then(|| now.checked_add(std::time::Duration::from_secs(snap.timeout_secs)))
        .flatten();
    let from_ctx = ctx.deadline.map(|dl| dl.max(now));
    match (from_cfg, from_ctx) {
        (Some(a), Some(b)) => Some(if a <= b {
            WallDeadline {
                at: a,
                from_cfg: true,
            }
        } else {
            WallDeadline {
                at: b,
                from_cfg: false,
            }
        }),
        (Some(a), None) => Some(WallDeadline {
            at: a,
            from_cfg: true,
        }),
        (None, Some(b)) => Some(WallDeadline {
            at: b,
            from_cfg: false,
        }),
        (None, None) => None,
    }
}

/// Hard cap on ONE host-side read of a guest `(ptr, len)` range (16 MiB).
/// The guest's own budgets (mem_mb, fuel) bound what it can build, but the
/// HOST copy is invisible to all of them — this cap keeps a runaway result
/// from becoming a runaway host allocation. Sized far above any sane result
/// (max_result_chars defaults to 60k) yet far below host-risk scale; the
/// overshoot case is reported loudly by the take_result/take_last_error
/// consumers, never silently truncated to "".
pub(super) const GUEST_READ_CAP_BYTES: usize = 1 << 24;

/// Read a `(ptr, len)` range out of linear memory as an owned String (lenient
/// lossy decode — the guest serializes UTF-8, but a truncated buffer from a
/// killed run must not panic the host). None = over [`GUEST_READ_CAP_BYTES`]
/// or unreadable range; every caller must surface that, not default it away.
pub(super) fn read_memory_string(
    mem: &Memory,
    store: &mut Store<HostCtx>,
    ptr: usize,
    len: usize,
) -> Option<String> {
    if len > GUEST_READ_CAP_BYTES {
        return None;
    }
    let mut buf = vec![0u8; len];
    mem.read(&*store, ptr, &mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuel_exhaustion_terminates_with_clear_error() {
        // a death loop burns this in well under a second
        let cfg = CodeConfig {
            fuel: 5_000_000,
            ..CodeConfig::default()
        };
        let out = run_code("while (true) {}", cfg);
        assert!(out.is_error);
        assert!(out.content.contains("fuel exhausted"), "{}", out.content);
    }

    #[test]
    fn memory_cap_terminates_with_clear_error() {
        // the module's linked initial — any growth is denied; keep fuel out
        // of the way so mem must fire
        let cfg = CodeConfig {
            mem_mb: 32,
            fuel: 100_000_000_000,
            ..CodeConfig::default()
        };
        let out = run_code(
            "const keep = []; while (true) keep.push(new Array(10000).fill(1));",
            cfg,
        );
        assert!(out.is_error);
        assert!(
            out.content.contains("memory limit exceeded"),
            "{}",
            out.content
        );
    }

    /// correctness-review n-5: the mem flag is a sticky latch, so the rc!=0
    /// attribution must ALSO require the error to look like the OOM
    /// InternalError — a guest that caught the OOM and then failed for an
    /// unrelated reason keeps its real error instead of being re-attributed
    /// to the memory limit.
    #[test]
    fn caught_oom_then_unrelated_error_keeps_real_error() {
        let cfg = CodeConfig {
            mem_mb: 32,
            fuel: 100_000_000_000,
            ..CodeConfig::default()
        };
        let out = run_code(
            r#"try {
                   const keep = [];
                   while (true) keep.push(new Array(10000).fill(1));   // OOM
               } catch (e) { /* swallowed; the latch stays set */ }
               throw new Error("unrelated failure");"#,
            cfg,
        );
        assert!(out.is_error);
        assert!(
            out.content.contains("JS error") && out.content.contains("unrelated failure"),
            "the guest's real error must survive the sticky mem latch: {}",
            out.content
        );
        assert!(
            !out.content.contains("memory limit exceeded"),
            "must not re-attribute the unrelated error to mem: {}",
            out.content
        );
    }

    /// Fuel-rate calibration (design §6 backfill). Ignored by default — run
    /// with `cargo test --features code --lib fuel_rate_calibration --
    /// --ignored --nocapture` after touching the wasm asset or wasmtime
    /// version, and refresh the §6 note if the rate shifts materially.
    #[test]
    #[ignore]
    fn fuel_rate_calibration() {
        let cfg = CodeConfig {
            fuel: 1_000_000_000,
            ..CodeConfig::default()
        };
        let t = std::time::Instant::now();
        let out = run_code(
            "let s = 0; for (let i = 0; i < 1e12; i++) s += i; return s;",
            cfg,
        );
        let elapsed = t.elapsed();
        assert!(out.is_error);
        assert!(out.content.contains("fuel exhausted"));
        let rate = 1_000_000_000f64 / elapsed.as_secs_f64();
        eprintln!(
            "[calibration] 1e9 fuel = {:?} ({:.0} fuel/s, {:.3} fuel/ns)",
            elapsed,
            rate,
            1f64 / elapsed.as_nanos() as f64 * 1_000_000_000f64
        );
        // Calibrated: 1e9 fuel ≈ 11.6s of pure interpreted loop in a debug
        // build (~8.7e7 fuel/s; §6 backfill) — tens-of-seconds scale, not
        // minutes; this lower bound only catches gross engine
        // misconfiguration (e.g. fuel silently not metered).
        assert!(elapsed.as_secs() >= 5, "1e9 fuel vanished in {elapsed:?}");
    }

    #[test]
    fn deep_recursion_terminates_with_attributed_error() {
        // quickjs-ng disables its own stack limit under __wasi__, so deep
        // recursion bottoms out on wasmtime's 384 KiB wasm-stack cap as a
        // trap — attributed via Trap::StackOverflow (§6 four-source flags),
        // not left as an opaque backtrace dump.
        let out = run_code(
            "function f(n) { return 1 + f(n + 1); }\nreturn f(0);",
            CodeConfig::default(),
        );
        assert!(out.is_error);
        assert!(
            out.content
                .contains("[run_code] terminated: call stack exhausted"),
            "{}",
            out.content.chars().take(200).collect::<String>()
        );
    }

    /// Test 10, wall-clock half (as reachable): a guest whose single tool
    /// call crosses the wall clock — the in-flight call RUNS TO COMPLETION
    /// (the honest boundary, FR-9), then the first epoch check after it
    /// returns traps the sandbox with the wall-clock message. The run must
    /// not silently succeed past its deadline. (The literal "guest keeps
    /// calling tools after the wall clock" reading is unreachable by
    /// construction: the epoch trap lands at the next dispatch backedge
    /// before the guest can issue another call — the `host_call_tool` entry
    /// check is that sub-tick window's defense-in-depth, and the latch half
    /// is pinned above via max_tool_calls=0.)
    #[test]
    fn wall_clock_fires_around_a_long_in_flight_tool_call() {
        assert!(!run_code("return 1", CodeConfig::default()).is_error);
        let dir = scratch();
        std::fs::write(dir.join("f.txt"), "x\n").unwrap();
        let cfg = CodeConfig {
            timeout_secs: 1,
            ..CodeConfig::default()
        };
        let out = tool_with_tools(cfg, r#"["read_file","shell"]"#)
            .run_with_emit(
                &json!({"code": r#"tools.call("shell", {command: "sleep 2"}); return "done";"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("wall clock exhausted"),
            "{}",
            out.content
        );
        // The honest boundary: the in-flight call itself completed and is
        // audited — not cancelled, not dropped.
        let meta = out.meta.expect("audit meta");
        assert_eq!(meta["call_count"], 1, "{meta}");
        assert_eq!(meta["calls"][0]["ok"], json!(true), "{meta}");
    }

    // ── M3: wall clock / cancel / JS-exception shape / D3 / A-10b ──────────

    /// A-4, wall clock leg: a busy loop is cut by code.timeout_secs with a
    /// source-tagged message. timeout_secs=1 keeps the test ~1s.
    #[test]
    fn wall_clock_terminates_busy_loop() {
        // Fuel out of the race: a tight bytecode loop charges fuel in
        // check-point chunks (~1e9/s in a debug build), so the default 1e9
        // budget and this 1s deadline expire nearly simultaneously and fuel
        // wins the adjudication race. 50e9 puts the wall clock alone in play.
        let cfg = CodeConfig {
            timeout_secs: 1,
            fuel: 50_000_000_000,
            ..CodeConfig::default()
        };
        // Warm the process-level OnceLocks (module compile is ~12s in a
        // debug build) so the timed window measures only delivery.
        assert!(!run_code("return 1", CodeConfig::default()).is_error);
        let started = Instant::now();
        let out = run_code("while (true) {}", cfg);
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("wall clock exhausted"),
            "source-tagged message: {}",
            out.content
        );
        assert!(
            out.content.contains("timeout_secs=1"),
            "message carries the knob: {}",
            out.content
        );
        assert!(
            started.elapsed().as_secs() < 5,
            "epoch delivery must be prompt, took {:?}",
            started.elapsed()
        );
        // The fuel was zeroed to FORCE the trap — fuel_used must report the
        // pre-zero snapshot, not the full budget (honest audit).
        let fuel = out.meta.expect("audit meta")["fuel_used"].as_u64().unwrap();
        assert!(
            fuel < 50_000_000_000,
            "interrupted run must not read as full burn: {fuel}"
        );
    }

    /// correctness-review M-2: a `timeout_secs` near u64::MAX overflows the
    /// clock on naive `now + Duration` — it must degrade to "no wall clock"
    /// (checked_add, mirroring agent.rs's limits.timeout_secs discipline and
    /// its huge_timeout_secs test), not panic the process.
    #[test]
    fn huge_timeout_secs_degrades_to_no_wall_clock() {
        let cfg = CodeConfig {
            timeout_secs: u64::MAX,
            ..CodeConfig::default()
        };
        let out = run_code("return \"clock-ok\";", cfg);
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "\"clock-ok\"");
    }

    /// A-5: the shared cancel flag cuts a busy loop within an epoch tick
    /// (~10ms after the flip), with the cancel-specific message and the
    /// documented in-flight boundary spelled out.
    #[test]
    fn cancel_flag_terminates_busy_loop() {
        use std::sync::atomic::AtomicBool;
        let flag = Arc::new(AtomicBool::new(false));
        let dir = scratch();
        let mut c = ctx(&dir);
        c.cancel = Some(flag.clone());
        // Warm first: a cold process pays the ~10s module compile, by which
        // time the flipper below would have already fired and the ENTRY gate
        // (not the in-flight delivery this test pins) would do the work.
        assert!(!run_code("return 1", CodeConfig::default()).is_error);
        let flipper = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let started = Instant::now();
        let out = tool_with(CodeConfig::default())
            .run_with_emit(&json!({"code": "while (true) {}"}), &c, &mut |_| {})
            .unwrap();
        flipper.join().unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("cancelled"),
            "cancel-specific message: {}",
            out.content
        );
        assert!(
            out.content.contains("in-flight"),
            "the honest boundary is part of the contract message: {}",
            out.content
        );
        assert!(
            started.elapsed().as_secs() < 5,
            "epoch delivery must be prompt, took {:?}",
            started.elapsed()
        );
    }

    /// Deterministic deadline leg (no 1s wait): an already-past ctx deadline
    /// fires on the first epoch tick — before the guest effectively runs.
    #[test]
    fn past_deadline_terminates_immediately() {
        let dir = scratch();
        let mut c = ctx(&dir);
        c.deadline = Some(Instant::now() - std::time::Duration::from_secs(1));
        let out = tool_with(CodeConfig::default())
            .run_with_emit(&json!({"code": "return 1;"}), &c, &mut |_| {})
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("wall clock exhausted"),
            "{}",
            out.content
        );
    }

    #[test]
    fn wasmtime_cache_dir_prefers_absolute_xdg_then_home() {
        let base = std::env::temp_dir().join(format!("ta-cache-{}", std::process::id()));
        let xdg = base.join("xdg");
        let home = base.join("home");
        std::fs::create_dir_all(&xdg).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        // Absolute XDG wins.
        let d = wasmtime_cache_dir_under(Some(xdg.clone()), Some(home.clone())).unwrap();
        assert_eq!(d, xdg.join("tinyagent").join("wasmtime"));
        // No XDG → HOME/.cache; a relative XDG is ignored like a missing one.
        let d = wasmtime_cache_dir_under(None, Some(home.clone())).unwrap();
        assert_eq!(d, home.join(".cache").join("tinyagent").join("wasmtime"));
        let d = wasmtime_cache_dir_under(Some("relative".into()), Some(home.clone())).unwrap();
        assert_eq!(d, home.join(".cache").join("tinyagent").join("wasmtime"));
        // Neither set (or an unusable path) → no cache, never an error.
        assert!(wasmtime_cache_dir_under(None, None).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}
