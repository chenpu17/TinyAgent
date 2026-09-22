//! `run_code`: LLM-written JavaScript executed in an in-process wasm sandbox.
//!
//! The guest is quickjs-ng compiled to `wasm32-wasi` WITHOUT quickjs-libc and
//! with every WASI symbol stubbed at link time, so the module's ONLY imports
//! are `env.host_call_tool` / `env.host_log` — capability surface is the two
//! functions, nothing ambient (docs/run_code_design.md §5). The host embeds it
//! via wasmtime: one Engine/Module/Linker per process, one Store per tool
//! execution, fuel + linear-memory caps per execution (§6).
//!
//! Guest↔host contract (§5.1): `alloc`/`dealloc` for buffer exchange,
//! `eval_code(code)` runs the code as a function body (top-level `return`
//! works), `take_result`/`take_last_error` hand back packed
//! `(ptr << 32) | len` slots. Each inner tool call goes through the SAME
//! gate → run → after chain as model-direct calls (FR-3): the hooks gate
//! fires first (unknown names included — matching the top-level order), then
//! the deny_paths / fs_write_dirs interception layer judges the
//! post-`Modify` EFFECTIVE input (§7), then the run. The ToolCall event is
//! emitted before the gate and the ToolResult right after the call returns
//! (H1: result emission belongs to the caller; the outer loop only assembles
//! results for ITS calls). Every call lands in the `meta` audit trace with
//! dual truncated-sha256 input hashes (§8.4) — original vs. effective — plus
//! call count / limit / fuel usage.
//!
//! The embedded asset is built by `examples/build-quickjs.sh` (source
//! provenance + sha256 recorded there). Everything through milestone M3 is
//! live: registry (whitelist + shared MCP tools), interception layer, audit
//! meta, call limit, the log cap + CodeLog events, the enable/config switch
//! face (§8.6), and the epoch-delivered wall clock / cancel.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use sha2::Digest;
use wasmtime::{Caller, Engine, Extern, Linker, Memory, Module, ResourceLimiter, Store, TypedFunc};

use crate::agent::{ctx_cancelled, gate_tool_call, notify_tool_after, run_tool, AgentEvent};
use crate::config::{CodeConfig, Config};
use crate::hooks::ToolDecision;
use crate::provider::{ToolDef, Usage};
use crate::tools::{mcp, SharedTool, Tool, ToolConcurrency, ToolCtx, ToolOrigin, ToolOutput};

/// The sandbox guest: quickjs-ng + glue (see module doc). Embedded so the
/// binary is self-contained — no filesystem dependency at run time.
const QUICKJS_WASM: &[u8] = include_bytes!("../../assets/quickjs.wasm");

/// Reserved tool name the guest's `tools.list()` routes to the host (§5.2).
/// Interception happens BEFORE the hooks gate, matching the design's
/// interception order (wall clock → reserved name → hooks → deny → run).
const TOOLS_LIST_NAME: &str = "__tools_list";
/// Reserved name behind `tools.budget()` (v2 review): read-only limits
/// introspection — calls left, wall clock left, fuel left. Same dispatch
/// class as [`TOOLS_LIST_NAME`] (no gate/audit/quota), so checking the
/// budget never spends it.
const BUDGET_NAME: &str = "__budget";
/// Reserved names of the lib family (code_memory_design MD-2, M1). Unlike
/// the two above these are REAL gated actions — host file reads under
/// policy — dispatched through [`lib_reserved_call`]: lib/ confinement →
/// deny_paths → hooks gate (pseudo-tool name without the dunder) → cancel
/// checkpoint → host work, audited and counted like any inner call. The
/// prelude wrappers `include()` / `saveLib()` route here; the guest may
/// also call the names directly (same checks either way).
const INCLUDE_NAME: &str = "__include";
const HASH_FILE_NAME: &str = "__hash_file";
/// Reserved name behind `tools.callBatch(entries)` (code_llm_design §4/LD-19,
/// M2): one batched dispatch of read-only calls. The reserved name is only
/// the ENTRANCE — every entry runs the exact single-call sequence (entry
/// checks → gate → deny/fs → run → after); nothing is exempted.
const CALL_BATCH_NAME: &str = "__call_batch";
/// Wave fan-out (§4): 8 per wave, aligned with the loop's parallel-batch
/// `MAX_PARALLEL_TOOL_WORKERS` so sandbox and outer fan-out share one
/// blast-radius convention.
const BATCH_WAVE: usize = 8;
/// Shape guard on one batch (not a spec'd number): the 1 MiB code cap
/// already bounds entries to ~10⁴; this keeps the per-entry error-envelope
/// build for a degenerate all-refused batch cheap and the guest's result
/// array sane. Larger work = smaller batches + checkpoint files.
const MAX_BATCH_ENTRIES: usize = 4096;
/// The manifest file the host parses for admission (MD-11): `lib/INDEX.md`,
/// machine lines `<!-- ta-lib: path=… sha256=… needs=… ver=… st=… -->`
/// (path relative to `lib/`). The HOST only ever parses; writes stay with
/// the caller (saveLib upserts via the ordinary file tools — 5.1's
/// counterexample 2 documents that a guest with write access can edit
/// INDEX; the session registration the admission actually trusts is
/// host-side and untouchable).
const LIB_INDEX_REL: &str = "lib/INDEX.md";
/// Guest-visible tombstone for "cancel observed, the sandbox is halting" —
/// shared by the host_call ENTRY gate (§13-5) and the post-hooks-gate cancel
/// checkpoint so the copy stays single-sourced (LLM-visible copy is spec;
/// see docs/finetune_support_design.md appendix A).
const CANCEL_HALTED_WHY: &str =
    "run_code: cancelled (host cancel flag) — the sandbox is halted for the rest of this execution";

/// Sub-agent identity tag for guest-originated tool calls (design D14): hooks
/// can tell "called from run_code" apart from top-level and `task` calls. The
/// `__` prefix is reserved — `task`'s name validation rejects guest-chosen
/// names starting with it, so the marker cannot be forged by a sub-agent.
const CODE_MARKER: &str = "__code__";

/// Hard cap on guest code size. Generous (≈ a 4k-token program); exists so a
/// runaway `code` argument fails with a clear error instead of an OOM in the
/// guest's allocator.
const MAX_CODE_BYTES: usize = 1024 * 1024;

/// The linked module starts at 32 MiB of linear memory (`--initial-memory`),
/// so a lower `mem_mb` cannot be honored — the floor is the module itself.
const MODULE_INITIAL_MEMORY: usize = 33_554_432;
/// Table-element ceiling for the memory limiter (see `table_growing`) —
/// generous vs. any legitimate funcref/ref table, far below host-risk scale.
const MAX_TABLE_ELEMENTS: usize = 1 << 20;

// ── process-level wasm singletons ──────────────────────────────────────────
//
// Init results are cached inside the OnceLock (`OnceLock<Result<T, String>>`)
// because `get_or_try_init` is not stable: a failed init is remembered, not
// retried per call — these are "cannot happen" paths (embedded asset) except
// the registry, whose failure (bad cfg.tools) is a config error best reported
// identically on every call.

fn engine() -> Result<&'static Engine> {
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
fn wasmtime_cache_dir() -> Option<std::path::PathBuf> {
    wasmtime_cache_dir_under(
        std::env::var_os("XDG_CACHE_HOME").map(std::path::PathBuf::from),
        std::env::var_os("HOME").map(std::path::PathBuf::from),
    )
}

/// Param-injectable core of [`wasmtime_cache_dir`] (env reads are process-
/// global and tests run in parallel); still performs the mkdir, which is the
/// operation whose failure decides the outcome.
fn wasmtime_cache_dir_under(
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
fn ensure_ticker() {
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
            eprintln!("run_code: epoch-ticker thread failed to spawn — wall-clock interruption disabled (fuel/mem limits still active)");
        }
    });
}

fn module() -> Result<&'static Module> {
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

fn linker() -> Result<&'static Linker<HostCtx>> {
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
                        let Some(line) = read_guest(caller.get_export("memory"), &caller, ptr, len)
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
struct MemLimiter {
    max_bytes: usize,
    exceeded: bool,
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
struct CallTrace {
    name: String,
    ok: bool,
    ms: u128,
    in_len: usize,
    in_hash: String,
    in_hash_eff: Option<String>,
}

/// Everything the imports need, reachable via `Store::data()`. Held in `Arc`s
/// so the struct is `'static` (Store data must be) while sharing the registry.
struct HostCtx {
    /// Event sink aliased from `run_with_emit`'s `emit` borrow via a lifetime-
    /// erased fat pointer (see the transmute there for the soundness window).
    emit: *mut (dyn FnMut(AgentEvent) + 'static),
    tools: Arc<Vec<Box<dyn Tool>>>,
    defs: Arc<Vec<ToolDef>>,
    ctx: ToolCtx,
    logs: Vec<String>,
    /// Chars collected into `logs` so far, and lines dropped past the 2×
    /// max_result_chars cap (§6 中-3) — together they bound host memory a
    /// log-flooding guest can cost.
    logs_chars: usize,
    logs_dropped: u64,
    limiter: MemLimiter,
    /// Usage billed by inner tool calls (`task`-style ToolOutput::usage),
    /// folded into run_code's own output so the outer loop counts it once.
    usage: Usage,
    /// The execution's config snapshot (deny lists, call limit, …).
    snap: CodeConfig,
    /// Canonicalized workdir — the base the interception layer relativizes
    /// guest paths against (§7). Computed once per execution.
    workdir_canon: PathBuf,
    /// Audit trace of every inner call (§8.4) — lands in `ToolOutput.meta`.
    calls: Vec<CallTrace>,
    /// Once set (call-limit breach / wall-clock exhaustion), every subsequent
    /// host_call_tool — including `__tools_list` — fails fast with this
    /// reason: an over-limit guest must not keep burning host time even in
    /// "cheap" calls (design §5.2 低-1 puts the entry check first). Carries
    /// the structured-error `kind` ("budget" / "deadline") alongside the
    /// message so the re-thrown Error keeps its class (v2 review).
    halted: Option<HaltState>,
    /// Hard wall clock (§6 execution point ①): start + min(code.timeout_secs,
    /// ctx.deadline remaining); `None` = no wall clock (both sources absent —
    /// cancel/fuel/mem still bound the run, the documented boundary). Carries
    /// which side bound it so the termination message names the right knob.
    deadline: Option<WallDeadline>,
    /// Set when cancel or the wall clock fires (§6 four-source flags — these
    /// two arrive via the epoch callback / entry check; fuel and mem have
    /// their own native signals). `terminated` maps it to a source-specific
    /// message.
    interrupt: Option<Interrupt>,
    /// Session registration set for the lib family (MD-14): `(workdir-relative
    /// canonical path, sha256 of delivered bytes)` pairs the session has
    /// registered via `__hash_file` (i.e. via `saveLib`). Clone-shared across
    /// every execution of one tool lineage, never persisted — a fresh
    /// Agent/process starts empty, `--resume` included ("clone 共享、不跨
    /// 会话恢复"). This is the ONLY admission input the guest cannot forge:
    /// the host reads and hashes the bytes itself.
    lib_sessions: LibSessions,
}

/// Which asynchronous limit cut the guest short (§6). Cancel and the wall
/// clock share the epoch-delivery path but stay distinguishable for the
/// error message and the audit story.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Interrupt {
    Cancel,
    Deadline,
}

impl HostCtx {
    fn new(
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
        }
    }
}

/// A latched halt (call limit / wall clock): the guest-visible message plus
/// the structured-error class for every subsequent failed call.
#[derive(Clone)]
struct HaltState {
    kind: &'static str,
    why: String,
}

// ── lib family: include / saveLib (code_memory_design M1) ───────────────────

/// Shared session registration set behind [`HostCtx::lib_sessions`] (see
/// there). `HashSet` over an `Arc<Mutex<_>>` so all clones of one
/// `RunCodeTool` lineage — and every execution they run — see one set.
type LibSessions = Arc<std::sync::Mutex<std::collections::HashSet<(String, String)>>>;

/// One parsed `ta-lib` line of `lib/INDEX.md` (MD-11). `needs`/`ver` are
/// carried for forward compatibility (M2+ routing) and round-tripping by
/// saveLib's upsert; M1 admission only consults `sha256` and `st`.
struct LibEntry {
    sha256: String,
    #[allow(dead_code)] // parsed + round-tripped by saveLib; read from M2 on
    needs: String,
    #[allow(dead_code)]
    ver: String,
    st: String,
}

/// Parse the manifest (MD-11). Strict by design — a malformed, duplicated or
/// conflicting entry is a HARD error, never a silent pick: INDEX is the
/// cross-session trust list, and "which entry won" is exactly the ambiguity
/// the design refuses. A missing file is the caller's case (→ "no entry"
/// admission error), not a parse error.
fn parse_lib_index(text: &str) -> Result<std::collections::HashMap<String, LibEntry>, String> {
    let mut map: std::collections::HashMap<String, LibEntry> = std::collections::HashMap::new();
    for (no, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if !(line.starts_with("<!-- ta-lib:") && line.ends_with("-->")) {
            continue; // prose/comments are free-form; only ta-lib lines parse
        }
        let body = line
            .strip_prefix("<!-- ta-lib:")
            .and_then(|s| s.strip_suffix("-->"))
            .unwrap_or("");
        let mut path: Option<String> = None;
        let mut sha256: Option<String> = None;
        let mut needs: Option<String> = None;
        let mut ver: Option<String> = None;
        let mut st: Option<String> = None;
        for tok in body.split_whitespace() {
            let (k, v) = tok.split_once('=').unwrap_or(("", ""));
            let v = v.to_string();
            match k {
                "path" if path.is_none() => path = Some(v),
                "sha256" if sha256.is_none() => sha256 = Some(v),
                "needs" if needs.is_none() => needs = Some(v),
                "ver" if ver.is_none() => ver = Some(v),
                "st" if st.is_none() => st = Some(v),
                _ => {
                    return Err(format!(
                        "line {}: unknown or duplicated field {:?}",
                        no + 1,
                        if k.is_empty() { tok } else { k }
                    ))
                }
            }
        }
        let (Some(path), Some(sha256), Some(needs), Some(ver), Some(st)) =
            (path, sha256, needs, ver, st)
        else {
            return Err(format!(
                "line {}: a ta-lib line needs all five fields \
                 (path/sha256/needs/ver/st)",
                no + 1
            ));
        };
        if path.is_empty()
            || path.starts_with('/')
            || path.contains("..")
            || path.split('/').any(|s| s.is_empty())
        {
            return Err(format!(
                "line {}: path must be a clean lib/-relative path",
                no + 1
            ));
        }
        if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("line {}: sha256 must be 64 hex chars", no + 1));
        }
        if st != "ok" && st != "draft" {
            return Err(format!(
                "line {}: st must be ok or draft, got {st:?}",
                no + 1
            ));
        }
        if ver.is_empty() || !ver.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!(
                "line {}: ver must be a number, got {ver:?}",
                no + 1
            ));
        }
        if map
            .insert(
                path.clone(),
                LibEntry {
                    sha256,
                    needs,
                    ver,
                    st,
                },
            )
            .is_some()
        {
            return Err(format!(
                "line {}: duplicate ta-lib entry for path {path:?}",
                no + 1
            ));
        }
    }
    Ok(map)
}

/// Full-hex sha256 of delivered bytes (MD-12 — hashed on what the guest will
/// actually receive, i.e. the lossy-decoded source, not the raw disk bytes:
/// invalid-UTF-8 files hash as they deliver).
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = sha2::Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// Confine a lib-family path: the workdir-relative normalized form, required
/// to sit under `lib/` (MD-2 ①). Symlink safety comes free from
/// [`workdir_relative`]'s canonicalize-based walk — `lib/x.js` symlinked
/// outside the workdir canonicalizes outside and yields `None`; absolute
/// spellings resolve from the workdir anchor like every other guest path.
/// Whitespace is refused too: `lib/INDEX.md` machine lines are
/// whitespace-tokenized, so a whitespace path can never round-trip through
/// the manifest — admitting it would let saveLib poison the whole channel.
fn lib_rel(workdir_canon: &Path, path: &str) -> Option<String> {
    let rel = workdir_relative(workdir_canon, path)?;
    (rel.starts_with("lib/")
        && rel.len() > "lib/".len()
        && !rel.ends_with('/')
        && !rel.chars().any(char::is_whitespace))
    .then_some(rel)
}

/// Dispatch for the lib-family reserved names — the Phase-B-shaped sequence
/// with the lib policy layers first (MD-2 order: ① lib/ confinement
/// ② deny_paths ③ hooks gate as pseudo-tool ④ trace) and the HOST doing the
/// work instead of `run_tool`. Emits the event pair, audits into `calls`,
/// counts against `max_tool_calls` (a probing guest pays like any caller).
/// Gate off (`code.include=false`): explicit refusal naming the knob, still
/// audited — the pre-guest contract of the old binary minus the probe.
fn lib_reserved_call(caller: &mut Caller<'_, HostCtx>, name: &str, args: &Value) -> i64 {
    let started = Instant::now();
    let (in_len, in_hash) = hash_input(args);
    // The pseudo-tool name the hooks gate / after_tool see ("include" /
    // "hash_file" — MD-2 ③): dunder names are the wire protocol, the
    // policy surface is the tool the action actually is.
    let pseudo = name.trim_start_matches("__");
    let (output, class) = {
        let d = caller.data();
        // SAFETY: same frame-limited alias as in `host_call_tool`.
        let emit = unsafe { &mut *d.emit };
        emit(AgentEvent::ToolCall {
            name: name.to_string(),
            input: args.clone(),
        });
        if !d.snap.include {
            (
                ToolOutput::error(format!(
                    "run_code: {name} is not available in this configuration \
                     (code.include=false) — the lib helpers are disabled"
                )),
                "denied",
            )
        } else {
            let path = args.get("path").and_then(Value::as_str);
            let confined = path.and_then(|p| lib_rel(&d.workdir_canon, p));
            // ① confinement + ② deny_paths — ahead of the hooks gate: a path
            // outside the lib root is a guest bug, not a policy question.
            let policy_err = match (&path, &confined) {
                (None, _) => Some("run_code: lib helpers need a string field 'path'".to_string()),
                (Some(p), None) => Some(format!(
                    "run_code: lib helpers only accept relative paths under lib/ \
                     ('{p}' is not one)"
                )),
                (Some(p), Some(rel)) => {
                    if deny_match(&d.snap.deny_paths, rel) {
                        Some(format!(
                            "run_code: path '{p}' matches a code.deny_paths pattern"
                        ))
                    } else {
                        None
                    }
                }
            };
            match policy_err {
                Some(err) => (ToolOutput::error(err), "denied"),
                None => match gate_tool_call(&d.ctx, pseudo, args) {
                    ToolDecision::Deny(mut reason) => {
                        crate::tools::truncate(&mut reason, d.ctx.max_output_chars);
                        let output = ToolOutput::error(reason);
                        notify_tool_after(&d.ctx, pseudo, args, &output, false);
                        (output, "hooks")
                    }
                    _ => {
                        if ctx_cancelled(&d.ctx) {
                            let output = ToolOutput::error(CANCEL_HALTED_WHY.to_string());
                            notify_tool_after(&d.ctx, pseudo, args, &output, false);
                            (output, "cancel")
                        } else {
                            let rel = confined.expect("policy_err None implies confined");
                            let path = path.expect("policy_err None implies path");
                            let (output, class) = if name == INCLUDE_NAME {
                                include_deliver(d, &rel, path)
                            } else {
                                hash_file_register(d, &rel)
                            };
                            notify_tool_after(&d.ctx, pseudo, args, &output, true);
                            (output, class)
                        }
                    }
                },
            }
        }
    };
    emit_inner_result(caller, name, &output);
    {
        let d = caller.data_mut();
        // ④ trace (MD-2): the wire name — audit records what the guest
        // actually called. No `in_hash_eff`: the lib family has no Modify
        // path (the path IS the input).
        d.calls.push(CallTrace {
            name: name.to_string(),
            ok: !output.is_error,
            ms: started.elapsed().as_millis(),
            in_len,
            in_hash,
            in_hash_eff: None,
        });
    }
    if output.is_error {
        envelope_err(caller, class, Some(name), Some(false), &output.content)
    } else {
        envelope_or_trap(caller, true, &output.content)
    }
}

/// Read a lib file with the HOST-side allocation capped at the lib read
/// limit: `take(MAX+1)` bounds the buffer even if the file is bigger on disk
/// (or grows mid-read). The wasm memory limiter cannot see host allocations,
/// so an unbounded slurp here would be an OOM side channel the sandbox
/// budgets know nothing about — `read_file` streams; this is the lib
/// channel's equivalent discipline.
fn read_lib_capped(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    file.take(MAX_CODE_BYTES as u64 + 1).read_to_end(&mut buf)?;
    Ok(buf)
}

/// `__include` (MD-14 admission): read the lib file, hash the DELIVERED
/// bytes, verify against the manifest entry, then admit on `st=ok` OR a
/// this-session registration. Every failure is a hard "lib" error — the
/// integrity chain is the feature; nothing degrades to "include anyway".
fn include_deliver(d: &HostCtx, rel: &str, spelling: &str) -> (ToolOutput, &'static str) {
    let err = |msg: String| (ToolOutput::error(msg), "lib");
    let manifest_rel = rel.strip_prefix("lib/").unwrap_or(rel);
    let bytes = match read_lib_capped(&d.workdir_canon.join(rel)) {
        Ok(b) => b,
        Err(e) => {
            return err(format!(
                "run_code: cannot read '{spelling}' for include: {e} — saveLib it first"
            ))
        }
    };
    if bytes.len() > MAX_CODE_BYTES {
        return err(format!(
            "run_code: '{spelling}' is {} bytes, over the {}-byte include read cap \
             — split the library (a lib file shares the size budget of one \
             run_code program)",
            bytes.len(),
            MAX_CODE_BYTES
        ));
    }
    let src = String::from_utf8_lossy(&bytes).into_owned();
    let hex = sha256_hex(src.as_bytes());
    let index_text =
        std::fs::read_to_string(d.workdir_canon.join(LIB_INDEX_REL)).unwrap_or_default();
    let index = match parse_lib_index(&index_text) {
        Ok(m) => m,
        Err(e) => return err(format!("run_code: {LIB_INDEX_REL} is malformed: {e}")),
    };
    let Some(entry) = index.get(manifest_rel) else {
        return err(format!(
            "run_code: '{spelling}' has no entry in {LIB_INDEX_REL} — register it \
             first with saveLib(\"{rel}\", src), then include it"
        ));
    };
    if entry.sha256 != hex {
        return err(format!(
            "run_code: '{spelling}' on disk does not match its {LIB_INDEX_REL} entry \
             (file sha256:{}…, registered sha256:{}…) — the library changed after \
             registration; re-run saveLib or restore the file",
            &hex[..16],
            &entry.sha256[..16]
        ));
    }
    let registered = d
        .lib_sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&(rel.to_string(), hex.clone()));
    if entry.st != "ok" && !registered {
        return err(format!(
            "run_code: '{spelling}' is st=draft and was not written in this session — \
             admission refused. saveLib it in this session, or have a human review \
             the library and set st=ok in {LIB_INDEX_REL}"
        ));
    }
    (ToolOutput::text(src), "tool")
}

/// `__hash_file` (MD-12/MD-14): hash the delivered bytes of a lib file and
/// REGISTER `(canonical rel path, sha256)` in the session set. This is the
/// host-side half of saveLib — the registration the admission actually
/// trusts, derived from file facts, never from guest self-report.
fn hash_file_register(d: &HostCtx, rel: &str) -> (ToolOutput, &'static str) {
    let bytes = match read_lib_capped(&d.workdir_canon.join(rel)) {
        Ok(b) => b,
        Err(e) => {
            return (
                ToolOutput::error(format!("run_code: cannot read '{rel}': {e}")),
                "lib",
            )
        }
    };
    if bytes.len() > MAX_CODE_BYTES {
        return (
            ToolOutput::error(format!(
                "run_code: '{rel}' is {} bytes, over the {}-byte lib read cap — \
                 split the library",
                bytes.len(),
                MAX_CODE_BYTES
            )),
            "lib",
        );
    }
    let src = String::from_utf8_lossy(&bytes).into_owned();
    let hex = sha256_hex(src.as_bytes());
    d.lib_sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((rel.to_string(), hex.clone()));
    (
        ToolOutput::text(json!({ "sha256": hex, "bytes": src.len() }).to_string()),
        "tool",
    )
}

// ── callBatch: read-only parallel batch (code_llm_design §4, M2) ─────────────

/// Wave-start refresh of the entry-gate latches, batch form (§4: every entry
/// runs the same 入口检查 as a single call; the wave boundary is where a
/// batch re-runs them). Mirrors `host_call_tool`'s Phase A exactly — cancel
/// and the wall clock latch `interrupt` + `halted` TOGETHER (a catchable
/// envelope must not clear the latch on retry) — but instead of failing the
/// whole call it hands back the halt state so the not-yet-started entries of
/// the wave can be refused individually (`kind`/`halted:true` entries; the
/// batch itself keeps its shape).
fn batch_wave_halt_refresh(d: &mut HostCtx) -> Option<HaltState> {
    if d.halted.is_some() {
        return d.halted.clone();
    }
    if d.interrupt.is_none() && ctx_cancelled(&d.ctx) {
        d.interrupt = Some(Interrupt::Cancel);
        d.halted = Some(HaltState {
            kind: "cancel",
            why: CANCEL_HALTED_WHY.to_string(),
        });
    } else if d.deadline.is_some_and(|w| Instant::now() >= w.at) && d.interrupt.is_none() {
        let why = format!(
            "run_code: {} — the sandbox is halted for the rest of this execution",
            deadline_why(&d.snap, d.deadline.map(|w| w.from_cfg).unwrap_or(false))
        );
        d.interrupt = Some(Interrupt::Deadline);
        d.halted = Some(HaltState {
            kind: "deadline",
            why,
        });
    }
    d.halted.clone()
}

/// What one announced batch entry carries until it resolves (the audit /
/// hooks tail needs the ORIGINAL input and both hashes — same fields a
/// single call's Phase B/C record).
struct BatchEntryMeta {
    name: String,
    args: Value,
    in_len: usize,
    in_hash: String,
    in_hash_eff: Option<String>,
    started: Instant,
}

/// The resolved outcome of one entry — `class` shapes the error envelope
/// ("tool" for a real run, the refusal classes otherwise); `halted` rides
/// only on latch-sourced refusals (LD-9: call-limit / deadline / cancel).
struct EntryFinal {
    output: ToolOutput,
    class: &'static str,
    halted: bool,
}

/// Worker → main funnel message: forwarded child events (bounded backpressure
/// channel) plus per-entry completions in true completion order.
enum WaveMsg {
    Ev(AgentEvent),
    Done(usize, ToolOutput),
    Cancelled(usize),
}

/// The after_tool policy for one batch entry completion: `Some(executed)`
/// fires the hook with that flag, `None` skips it — reserved for the
/// deny/fs layer, which is not a hooks decision and never fires `after`
/// (the Hooks contract, mirrored from the single-call Phase B).
type AfterFlag = Option<bool>;

/// Complete one announced batch entry on the main thread — the exact
/// Phase B/C tail of a single call: `after_tool` (original input, the
/// policy above), the ToolResult event, the audit trace, the usage fold.
/// EVERY announced entry goes through here, refused ones included (external
/// design-review F1): an announced ToolCall without its ToolResult breaks
/// the event pairing discipline, and a refused entry that skips the trace
/// leaks quota — `admitted` counted it but `calls.len()` never sees it, so
/// post-batch single calls admit past `max_tool_calls` (the single-call
/// Deny/unknown/fs refusals all push their trace; batch parity demands the
/// same). Consumes the entry's meta.
fn finish_batch_entry(
    caller: &mut Caller<'_, HostCtx>,
    meta: BatchEntryMeta,
    output: &ToolOutput,
    after: AfterFlag,
) {
    if let Some(executed) = after {
        let d = caller.data();
        notify_tool_after(&d.ctx, &meta.name, &meta.args, output, executed);
    }
    emit_inner_result(caller, &meta.name, output);
    let d = caller.data_mut();
    d.calls.push(CallTrace {
        name: meta.name,
        ok: !output.is_error,
        ms: meta.started.elapsed().as_millis(),
        in_len: meta.in_len,
        in_hash: meta.in_hash,
        in_hash_eff: meta.in_hash_eff,
    });
    fold_inner_usage(d, output);
}

/// Refuse an announced entry and complete it through the same tail
/// ([`finish_batch_entry`]) — the entry was announced and gated, so it owes
/// the event stream its ToolResult and the audit ledger its trace whatever
/// the refusal class; only the after_tool policy varies by class. The final
/// outcome travels as the [`EntryFinal`] it becomes.
fn refuse_batch_entry(
    caller: &mut Caller<'_, HostCtx>,
    metas: &mut [Option<BatchEntryMeta>],
    finals: &mut [Option<EntryFinal>],
    idx: usize,
    final_: EntryFinal,
    after: AfterFlag,
) {
    let output = final_.output.clone();
    let meta = metas[idx].take().expect("announced entry has meta");
    finals[idx] = Some(final_);
    finish_batch_entry(caller, meta, &output, after);
}

/// One entry's D12 envelope for the result array (§4: the return shape is an
/// ARRAY of envelopes, not one envelope wrapping an array). Success carries
/// only `content`; errors carry `kind` (+ `tool`), `retryable:false` for
/// every non-"tool" class, and `halted:true` on latch-sourced refusals.
fn batch_entry_envelope(name: Option<&str>, f: &EntryFinal) -> Value {
    if f.output.image.is_some() {
        // The call RAN — side effects happened — the envelope is only the
        // transport-level inability to carry an image (same text as the
        // single-call Phase C; retryable:false too — a batch entry cannot
        // retry its way around a transport limit).
        let mut e = json!({
            "__ok": false,
            "error": "run_code: this tool returned an image, which run_code cannot carry — \
                      call read_file on the path instead, or use load_image at the top level",
            "kind": "image",
            "retryable": false,
        });
        if let Some(n) = name {
            e["tool"] = json!(n);
        }
        return e;
    }
    if f.output.is_error {
        let mut e = json!({ "__ok": false, "error": f.output.content, "kind": f.class });
        if let Some(n) = name {
            e["tool"] = json!(n);
        }
        if f.class != "tool" {
            e["retryable"] = json!(false);
        }
        if f.halted {
            e["halted"] = json!(true);
        }
        return e;
    }
    json!({ "__ok": true, "content": f.output.content })
}

/// Dispatch for the `__call_batch` reserved name — `tools.callBatch(entries)`
/// (§4). The reserved name is only the ENTRANCE: every entry runs the exact
/// single-call sequence (entry checks → ToolCall event → hooks gate →
/// deny/fs layer → run → after_tool → trace → usage fold), nothing exempted.
///
/// Structure per wave of [`BATCH_WAVE`] entries (aligned with the loop's
/// `MAX_PARALLEL_TOOL_WORKERS`):
/// - prep is SERIAL on the main thread, in submission order — entry checks
///   (halted / call-limit with latch / shape), the ToolCall event, the hooks
///   gate ("approval decisions never race", the loop's spawn-gate contract),
///   the cancel checkpoint, the `batchable()` + concurrency routing, and the
///   deny/fs layer;
/// - Sequential-batchable entries (v1: `read_file`, MCP readOnlyHint) then
///   run serially on the main thread — LD-11 routes the wave ENGINE by
///   `ToolConcurrency::Parallel`, and v1 has no Parallel batchable tool yet
///   (M3's `llm` is the first), so the common path is an honest serial batch
///   with the same per-entry contract;
/// - Parallel-batchable entries spawn one worker each; events funnel through
///   a bounded `sync_channel` created inside the scope closure (the unwind
///   discipline from the loop's parallel batch), workers re-check cancel
///   before the tool body, and completions (ToolResult, after_tool, trace,
///   usage) are processed on the main thread as they arrive — true
///   completion order for the events, submission order for the result array.
///
/// No transaction semantics (§4, the three sentences): no atomicity, no
/// rollback, partial execution is a normal result — the result array shows
/// exactly which entries ran and which were refused.
fn call_batch_dispatch(caller: &mut Caller<'_, HostCtx>, args: &Value) -> i64 {
    // Whole-call shape: a malformed BATCH is a whole-call `kind="batch"`
    // error (thrown to the guest by the prelude wrapper); malformed ENTRIES
    // are per-entry outcomes below.
    let entries: Vec<Value> = match args.get("entries").and_then(Value::as_array) {
        Some(a) if a.len() <= MAX_BATCH_ENTRIES => a.clone(),
        _ => {
            return envelope_err(
                caller,
                "batch",
                Some(CALL_BATCH_NAME),
                Some(false),
                &format!(
                    "run_code: __call_batch needs {{entries: [{{name, args}}, …]}} with at \
                     most {MAX_BATCH_ENTRIES} items"
                ),
            )
        }
    };
    let n = entries.len();
    let mut finals: Vec<Option<EntryFinal>> = (0..n).map(|_| None).collect();
    let mut metas: Vec<Option<BatchEntryMeta>> = (0..n).map(|_| None).collect();
    // Call-quota arithmetic (§4: K entries count as K calls). Admitted-but-
    // unfinished entries have not pushed their trace yet, so the live
    // `calls.len()` alone undercounts — track admissions against the budget
    // left at dispatch (Phase A already admitted this call itself). The
    // snapshot also feeds the refusal MESSAGE: during later waves the live
    // `calls.len()` already contains this batch's finished traces, so
    // recomputing `used` from it would double-count every completed entry.
    let initial_calls = caller.data().calls.len() as u64;
    let budget_left = (caller.data().snap.max_tool_calls as u64).saturating_sub(initial_calls);
    let mut admitted: u64 = 0;
    let order: Vec<usize> = (0..n).collect();

    for wave in order.chunks(BATCH_WAVE) {
        // Wave-start latch refresh (cancel / wall clock) — the entries of
        // this wave inherit the halt state as per-entry refusals.
        let halted = batch_wave_halt_refresh(caller.data_mut());

        // ── prep: serial, submission order ────────────────────────────────
        // (idx, name, effective input) — workers get the post-Modify input;
        // hooks and traces always see the original.
        let mut spawn: Vec<(usize, String, Value)> = Vec::new();
        let mut inline: Vec<(usize, String, Value)> = Vec::new();
        for &idx in wave {
            let entry = &entries[idx];
            let name = entry.get("name").and_then(Value::as_str);
            // Entry checks, single-call order: halted → call limit → shape.
            if let Some(h) = &halted {
                finals[idx] = Some(EntryFinal {
                    output: ToolOutput::error(h.why.clone()),
                    class: h.kind,
                    halted: true,
                });
                continue;
            }
            if admitted >= budget_left {
                // First over-budget admission latches (single-call semantics:
                // refusal latches) so post-batch calls fail fast too.
                // `initial_calls + admitted` = dispatch-time calls + every
                // entry announced so far — the count this refusal makes true.
                let used = initial_calls + admitted;
                let max = caller.data().snap.max_tool_calls;
                let why = call_limit_exceeded_why(used as usize, max);
                caller.data_mut().halted = Some(HaltState {
                    kind: "budget",
                    why: why.clone(),
                });
                finals[idx] = Some(EntryFinal {
                    output: ToolOutput::error(why),
                    class: "budget",
                    halted: true,
                });
                continue;
            }
            let (Some(name), Ok(args_v)) = (
                name,
                entry.get("args").map_or(Ok(Value::Null), |a| {
                    if a.is_object() {
                        Ok(a.clone())
                    } else {
                        Err(())
                    }
                }),
            ) else {
                finals[idx] = Some(EntryFinal {
                    output: ToolOutput::error(
                        "run_code: batch entries must be {name: \"tool name\", args: {…}} — \
                         use tools.list() for names"
                            .to_string(),
                    ),
                    class: "batch",
                    halted: false,
                });
                continue;
            };
            // Missing args defaults to {} (the single-call glue's
            // `undefined → {}` parity); a present-but-non-object args is the
            // shape error above.
            let entry_args = if args_v.is_null() { json!({}) } else { args_v };
            admitted += 1; // every announced entry counts (failures too, §6)

            // Announce + gate + intercept — the Phase B sequence, per entry.
            let (decision, started, in_hash) = {
                let d = caller.data();
                // SAFETY: same frame-limited alias as in `host_call_tool`.
                let emit = unsafe { &mut *d.emit };
                emit(AgentEvent::ToolCall {
                    name: name.to_string(),
                    input: entry_args.clone(),
                });
                let started = Instant::now();
                let (_, in_hash) = hash_input(&entry_args);
                (gate_tool_call(&d.ctx, name, &entry_args), started, in_hash)
            };
            metas[idx] = Some(BatchEntryMeta {
                name: name.to_string(),
                args: entry_args.clone(),
                in_len: entry_args.to_string().len(),
                in_hash,
                in_hash_eff: None,
                started,
            });
            let effective = match &decision {
                ToolDecision::Modify(v) => v.clone(),
                _ => entry_args.clone(),
            };
            match decision {
                ToolDecision::Deny(mut reason) => {
                    let cap = caller.data().ctx.max_output_chars;
                    crate::tools::truncate(&mut reason, cap);
                    refuse_batch_entry(
                        caller,
                        &mut metas,
                        &mut finals,
                        idx,
                        EntryFinal {
                            output: ToolOutput::error(reason),
                            class: "hooks",
                            halted: false,
                        },
                        Some(false),
                    );
                }
                _ => {
                    // Cancel checkpoint between the gate and execution — the
                    // announced entry still completes its event pair.
                    if ctx_cancelled(&caller.data().ctx) {
                        let output = ToolOutput::error(CANCEL_HALTED_WHY.to_string());
                        refuse_batch_entry(
                            caller,
                            &mut metas,
                            &mut finals,
                            idx,
                            EntryFinal {
                                output,
                                class: "cancel",
                                halted: true,
                            },
                            Some(false),
                        );
                        continue;
                    }
                    // Registry lookup + the two capability axes: batchable
                    // (LD-19) and concurrency (LD-11). Non-batchable → the
                    // entry-level kind="batch" error (单条不中止，与 Deny 同
                    // 语义：after_tool(executed=false) 照发，批次继续).
                    let tool_idx = caller.data().defs.iter().position(|t| t.name == name);
                    let Some(tool_idx) = tool_idx else {
                        // Mirror the single-call unknown path (run_tool's
                        // message, after_tool executed=true — dispatch
                        // happened, the tool did not exist).
                        let output = ToolOutput::error(format!("unknown tool: {name}"));
                        refuse_batch_entry(
                            caller,
                            &mut metas,
                            &mut finals,
                            idx,
                            EntryFinal {
                                output,
                                class: "unknown_tool",
                                halted: false,
                            },
                            Some(true),
                        );
                        continue;
                    };
                    {
                        let d = caller.data();
                        let tool = &d.tools[tool_idx];
                        if !tool.batchable() {
                            let output = ToolOutput::error(format!(
                                "run_code: {name} is not batchable — callBatch admits \
                                 read-only tools only (batchable()); side effects belong \
                                 in a sequential for loop of tools.call"
                            ));
                            refuse_batch_entry(
                                caller,
                                &mut metas,
                                &mut finals,
                                idx,
                                EntryFinal {
                                    output,
                                    class: "batch",
                                    halted: false,
                                },
                                Some(false),
                            );
                            continue;
                        }
                        // Dual-hash audit (§8.4): effective hash only when a
                        // Modify actually changed the input.
                        if effective != entry_args {
                            metas[idx].as_mut().expect("meta set above").in_hash_eff =
                                Some(hash_input(&effective).1);
                        }
                    }
                    // deny/fs layer on the EFFECTIVE input — not a hooks
                    // decision, after_tool does not fire (Hooks contract,
                    // same as the single-call Phase B).
                    if let Some(err) = intercept_file_paths(
                        name,
                        &effective,
                        &caller.data().snap,
                        &caller.data().workdir_canon,
                    ) {
                        refuse_batch_entry(
                            caller,
                            &mut metas,
                            &mut finals,
                            idx,
                            EntryFinal {
                                output: ToolOutput::error(err),
                                class: "denied",
                                halted: false,
                            },
                            None,
                        );
                        continue;
                    }
                    // LD-11 routing: the wave engine takes Parallel tools;
                    // Sequential batchable entries run serially below.
                    let parallel =
                        caller.data().tools[tool_idx].concurrency() == ToolConcurrency::Parallel;
                    if parallel {
                        spawn.push((idx, name.to_string(), effective));
                    } else {
                        inline.push((idx, name.to_string(), effective));
                    }
                }
            }
        }

        // ── execution: inline (Sequential) first, then the worker wave ────
        // (the loop's own discipline: its inline leaf pass also completes
        // before the parallel batch spawns — same order, same reasoning.)
        {
            let (tools, defs, ctx) = {
                let d = caller.data();
                (Arc::clone(&d.tools), Arc::clone(&d.defs), d.ctx.clone())
            };
            let emit_ptr = caller.data().emit;
            for (idx, name, input) in inline {
                // Pre-body cancel re-check — the inline twin of the worker
                // checkpoint below.
                if ctx_cancelled(&ctx) {
                    let output = ToolOutput::error(CANCEL_HALTED_WHY.to_string());
                    refuse_batch_entry(
                        caller,
                        &mut metas,
                        &mut finals,
                        idx,
                        EntryFinal {
                            output,
                            class: "cancel",
                            halted: true,
                        },
                        Some(false),
                    );
                    continue;
                }
                // SAFETY: no workers exist yet — the emit alias is used from
                // this thread only.
                let mut emit = |ev: AgentEvent| unsafe {
                    (&mut *emit_ptr)(ev);
                };
                let output = run_tool(&tools, &defs, &name, &input, &ctx, &mut emit);
                let meta = metas[idx].take().expect("inline entry was announced");
                finals[idx] = Some(EntryFinal {
                    output: output.clone(),
                    class: "tool",
                    halted: false,
                });
                finish_batch_entry(caller, meta, &output, Some(true));
            }

            if !spawn.is_empty() {
                std::thread::scope(|s| {
                    // The channel is created INSIDE the scope closure so `rx`
                    // is a closure local: on a drain-loop unwind the receiver
                    // drops, worker sends start failing, and scope's join-all
                    // can complete (borrowed-from-outside `rx` + blocked
                    // senders = deadlock — the loop's parallel batch hit this
                    // as re-review P2). The guard trips the shared cancel
                    // flag on unwind so in-flight workers wind down fast.
                    struct CancelOnUnwind {
                        flag: Option<Arc<std::sync::atomic::AtomicBool>>,
                        armed: bool,
                    }
                    impl Drop for CancelOnUnwind {
                        fn drop(&mut self) {
                            if self.armed && std::thread::panicking() {
                                if let Some(c) = &self.flag {
                                    c.store(true, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    let (tx, rx) = std::sync::mpsc::sync_channel::<WaveMsg>(256);
                    let mut unwind_guard = CancelOnUnwind {
                        flag: ctx.cancel.clone(),
                        armed: true,
                    };
                    let mut handles = Vec::with_capacity(spawn.len());
                    for (idx, name, input) in &spawn {
                        let tools = Arc::clone(&tools);
                        let defs = Arc::clone(&defs);
                        let ctx = ctx.clone();
                        let name = name.clone();
                        let input = input.clone();
                        let tx = tx.clone();
                        let idx = *idx;
                        handles.push(s.spawn(move || {
                            let mut emit = |ev: AgentEvent| {
                                // Blocks when the funnel is full — the drain
                                // loop in this same scope is the only
                                // consumer, so it always unwinds.
                                let _ = tx.send(WaveMsg::Ev(ev));
                            };
                            if ctx_cancelled(&ctx) {
                                let _ = tx.send(WaveMsg::Cancelled(idx));
                                return;
                            }
                            let output = run_tool(&tools, &defs, &name, &input, &ctx, &mut emit);
                            let _ = tx.send(WaveMsg::Done(idx, output));
                        }));
                    }
                    drop(tx); // workers hold the last clones; rx ends when they drop
                              // Drain on the main thread: forward child events live and
                              // complete each entry as its Done/Cancelled arrives —
                              // completion order for events, slots keep submission
                              // order for the final array.
                    while let Ok(msg) = rx.recv() {
                        match msg {
                            WaveMsg::Ev(ev) => unsafe {
                                // SAFETY: workers never touch the sink; the
                                // alias is main-thread-only (same window as
                                // host_call_tool).
                                (&mut *emit_ptr)(ev);
                            },
                            WaveMsg::Done(idx, output) => {
                                let meta = metas[idx].take().expect("worker entry was announced");
                                finals[idx] = Some(EntryFinal {
                                    output: output.clone(),
                                    class: "tool",
                                    halted: false,
                                });
                                finish_batch_entry(caller, meta, &output, Some(true));
                            }
                            WaveMsg::Cancelled(idx) => {
                                let output = ToolOutput::error(CANCEL_HALTED_WHY.to_string());
                                let meta = metas[idx].take().expect("worker entry was announced");
                                finals[idx] = Some(EntryFinal {
                                    output: output.clone(),
                                    class: "cancel",
                                    halted: true,
                                });
                                finish_batch_entry(caller, meta, &output, Some(false));
                            }
                        }
                    }
                    unwind_guard.armed = false;
                    // Join in spawn order: a panicked worker never sent Done —
                    // it gets the loop's placeholder shape (an honest error
                    // entry, after_tool(executed=true) on the dispatch that
                    // did happen). The `is_none` guard keeps a pathological
                    // panic-after-Done from overwriting (or double-finishing)
                    // the already-recorded outcome.
                    for ((idx, _, _), h) in spawn.iter().zip(handles) {
                        if h.join().is_err() && finals[*idx].is_none() {
                            let output = ToolOutput::error("batch tool worker panicked");
                            let meta = metas[*idx].take().expect("worker entry was announced");
                            finals[*idx] = Some(EntryFinal {
                                output: output.clone(),
                                class: "tool",
                                halted: false,
                            });
                            finish_batch_entry(caller, meta, &output, Some(true));
                        }
                    }
                });
            }
        }
    }

    // Assemble the result array in submission order (§4: the return shape is
    // the array of per-entry envelopes).
    let arr: Vec<Value> = (0..n)
        .map(|i| {
            let f = finals[i]
                .take()
                .expect("every entry resolves to a final outcome");
            let name = entries[i].get("name").and_then(Value::as_str);
            batch_entry_envelope(name, &f)
        })
        .collect();
    envelope_or_trap(caller, true, &Value::Array(arr).to_string())
}

/// The guest-visible tool registry (§2): the agent's tool surface filtered
/// to what each tool EXPLICITLY offers the sandbox (`available_to_code`,
/// v2 review) — the file 四件套 + shell + shared MCP tools opt in; `task`
/// (recursion), `todo` (planning belongs to the parent), run_code itself
/// (nested sandboxes buy nothing and burn memory), skill tools and any
/// unannotated third-party tool stay out. Host-injected extra tools are
/// admitted only on their own opt-in. Plus the guest-only primitives
/// (`copy_bytes`). Built lazily on first use and cached for the tool's
/// lifetime; built once per TOOL LINEAGE in M2 (§8.2) — the `Arc<OnceLock>`
/// crosses clones, and batch workers each assemble their own tool, so
/// per-process is accurate only for the single-agent case.
struct Registry {
    tools: Arc<Vec<Box<dyn Tool>>>,
    defs: Arc<Vec<ToolDef>>,
}

// ── import bodies ──────────────────────────────────────────────────────────

/// Split a packed `(ptr << 32) | len` return from the guest.
fn unpack(packed: i64) -> (usize, usize) {
    ((packed >> 32) as u32 as usize, packed as u32 as usize)
}

/// Read a byte range the guest points at. `None` on null/oversized/OOB — the
/// guest is untrusted, so every pointer/length pair is validated before use.
fn read_guest(
    mem: Option<Extern>,
    caller: &Caller<'_, HostCtx>,
    ptr: i32,
    len: i32,
) -> Option<String> {
    if !(0..=(1 << 20)).contains(&len) {
        return None;
    }
    if len == 0 {
        return Some(String::new());
    }
    if ptr <= 0 {
        return None;
    }
    let mem: Memory = mem?.into_memory()?;
    let mut buf = vec![0u8; len as usize];
    mem.read(caller, ptr as usize, &mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Write `bytes` into fresh guest memory via the guest's own `alloc` export
/// and return the packed pointer envelope. Re-entering wasm from a host
/// import is sound here: `alloc` is a plain malloc wrapper whose own imports
/// are these two functions, and malloc never calls the host — no reentrancy
/// into the intercepted call chain is possible.
fn write_guest(caller: &mut Caller<'_, HostCtx>, bytes: &[u8]) -> Result<i64> {
    let alloc: TypedFunc<i32, i32> = caller
        .get_export("alloc")
        .and_then(Extern::into_func)
        .context("guest exports alloc")?
        .typed(&mut *caller)?;
    let ptr = alloc.call(&mut *caller, bytes.len() as i32)?;
    if ptr == 0 {
        // malloc failure — 0 is the guest's static area, not a buffer it
        // handed back; writing there would corrupt globals, not deliver data.
        anyhow::bail!("guest alloc returned 0 (heap exhausted)");
    }
    let mem = caller
        .get_export("memory")
        .and_then(Extern::into_memory)
        .context("guest exports memory")?;
    mem.write(&mut *caller, ptr as usize, bytes)?;
    Ok((((ptr as u32 as u64) << 32) | bytes.len() as u64) as i64)
}

/// The `env.host_call_tool` import — the sandbox's entire outbound surface.
///
/// Interception order (design §7, fixed by review): entry checks (halted /
/// call limit — BEFORE the reserved names, so a busy loop can't sidestep the
/// audit cap via `tools.list`) → reserved-name intercepts (`__tools_list`,
/// `__budget` — read-only introspection, not tool calls; `__include` /
/// `__hash_file` — gated host actions with their own dispatch) → hooks gate
/// (unknown names gate too — same order as the top-level loop) → deny_paths /
/// fs_write_dirs on the post-`Modify` EFFECTIVE input → run.
/// Parse the guest's serialized tool arguments. The shipped glue always sends
/// valid JSON (`tools.call` stringifies its argument; `undefined` becomes the
/// literal `{}`; cyclic/BigInt values throw INSIDE the guest), so a parse
/// failure means a guest bypassed the JS wrapper and drove the raw wasm import
/// with garbage bytes — a protocol violation, reported as a structured `host`
/// error (symmetric with the bad-pointer errors in Phase A), never silently
/// degraded to `{}`: a missing-args tool run would misattribute the guest's
/// bug to the tool.
fn parse_guest_args(raw: &str) -> Result<Value, String> {
    serde_json::from_str(raw).map_err(|e| format!("tool arguments are not valid JSON: {e}"))
}

/// The call-limit refusal text — model-visible spec, byte-identical at every
/// emission site (the single-call entry check and the batch's per-entry
/// admission check share it).
///
/// 实测教训（TinyAgentBench AppWorld 复核）：模型连撞两次后曾误判"额度是整个
/// 会话累计的"而放弃沙箱路线。报错必须说清三件事：额度按次独立（下次 run_code
/// 重新计满）、超了怎么恢复（分批 + 中间结果落盘续跑）、失败/写错名的调用也
/// 照扣（先 tools.list）。
fn call_limit_exceeded_why(used: usize, max: u32) -> String {
    format!(
        "run_code: tool call limit exceeded ({used} of max_tool_calls={max}) — \
         the sandbox is halted for the rest of this execution. \
         The budget resets every invocation: the next run_code call \
         gets a fresh {max} calls. Split the work into batches of ≤{max} \
         calls and persist intermediate results with write_file so \
         the next batch can resume. Failed calls (including wrong \
         tool names) count too — check tools.list() before guessing."
    )
}

/// Saturating usage fold for inner-call billing (same shape as the core's):
/// share leaf-reported usage into the run-level budget counter, then merge
/// into the sandbox's aggregate. Shared by the single-call dispatch, the
/// batch's inline entries, and the batch's worker completions — one fold, no
/// local re-implementations drifting apart.
fn fold_inner_usage(d: &mut HostCtx, output: &ToolOutput) {
    // Honest cost fold (FR-10): inner tool usage bills to this run via the
    // shared saturating merge (compress.rs). Leaf-reported usage
    // (usage_in_shared_budget=false — e.g. a host callback tool metering an
    // upstream API) is shared into the budget counter HERE, at the innermost
    // fold: run_code reports the aggregate upward with the flag set
    // (everything in it is now counter-shared), and inner `task` children's
    // spend was already shared by their own chats — so nothing double-counts
    // on the way up.
    if let Some(u) = &output.usage {
        if !output.usage_in_shared_budget {
            if let Some(g) = &d.ctx.budget {
                g.share(u.total());
            }
        }
    }
    if let Some(merged) =
        crate::compress::merge_usage(Some(std::mem::take(&mut d.usage)), output.usage)
    {
        d.usage = merged;
    }
}

fn host_call_tool(
    caller: &mut Caller<'_, HostCtx>,
    name_ptr: i32,
    name_len: i32,
    args_ptr: i32,
    args_len: i32,
) -> i64 {
    // Phase A — copy the guest's bytes out (immutable borrows only).
    let mem = caller.get_export("memory");
    // Symmetric explicit errors for both pointers (sec-review n-1): a bad
    // name pointer must report itself, not silently become "" → "unknown
    // tool" hiding the corruption.
    let name = match read_guest(mem.clone(), caller, name_ptr, name_len) {
        Some(s) => s,
        None => {
            return envelope_err(
                caller,
                "host",
                None,
                Some(false),
                "run_code: bad name pointer from guest",
            )
        }
    };
    let args_raw = match read_guest(mem, caller, args_ptr, args_len) {
        Some(s) => s,
        None => {
            return envelope_err(
                caller,
                "host",
                Some(&name),
                Some(false),
                "run_code: bad args pointer from guest",
            )
        }
    };
    let args: Value = match parse_guest_args(&args_raw) {
        Ok(v) => v,
        Err(why) => {
            return envelope_err(
                caller,
                "host",
                Some(&name),
                Some(false),
                &format!("run_code: {why}"),
            )
        }
    };

    // Entry checks. Halted → everything fails fast, reserved name included
    // (§5.2). The call limit counts every REAL call (failures included, §6):
    // the check runs before execution, so `max_tool_calls` = N means the
    // first N calls run and call N+1 is refused — and refusal latches.
    // The wall-clock check (§6 execution point ②) runs FIRST of all — ahead
    // of the reserved names (§7's 低-1): a busy loop can neither sidestep the
    // audit cap nor outlive the deadline via `tools.list`.
    {
        let d = caller.data_mut();
        if let Some(h) = d.halted.clone() {
            return envelope_err(caller, h.kind, Some(&name), Some(false), &h.why);
        }
        // Cancel — the host_call ENTRY half of the §13-5 entry gate: the
        // epoch callback delivers the interrupt only at the next ~10ms check
        // point, and a guest call STARTED inside that window would run a real
        // inner tool for its full duration, past the documented boundary
        // ("in-flight calls run to completion" promises calls already
        // running, not new admissions). Latch the interrupt so `terminated`
        // attributes the following trap to cancel. Terminal (retryable=false)
        // — the whole execution is stopping, not this one call.
        if d.interrupt.is_none()
            && d.ctx
                .cancel
                .as_ref()
                .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
        {
            d.interrupt = Some(Interrupt::Cancel);
            let why = CANCEL_HALTED_WHY.to_string();
            // Halt the sandbox too (external re-review P1): this error is
            // JS-CATCHABLE, and a guest that catches it and retries re-enters
            // with `interrupt` already latched — the old `interrupt.is_none()`
            // guard then SKIPPED the cancel check and the retried call ran a
            // real inner tool until the next ~10ms epoch trap. Same shape as
            // the deadline arm below: latch interrupt (attribution) AND
            // halted (admission refusal) together, so any already-latched
            // interrupt state rejects every subsequent call.
            d.halted = Some(HaltState {
                kind: "cancel",
                why: why.clone(),
            });
            return envelope_err(caller, "cancel", Some(&name), Some(false), &why);
        }
        if d.deadline.is_some_and(|w| Instant::now() >= w.at) && d.interrupt.is_none() {
            // Name the knob that actually bound the run (correctness-review
            // m-1) — the parent run's remaining time may be the tighter side.
            let why = format!(
                "run_code: {} — the sandbox is halted for the rest of this execution",
                deadline_why(&d.snap, d.deadline.map(|w| w.from_cfg).unwrap_or(false))
            );
            d.interrupt = Some(Interrupt::Deadline);
            d.halted = Some(HaltState {
                kind: "deadline",
                why: why.clone(),
            });
            return envelope_err(caller, "deadline", Some(&name), Some(false), &why);
        }
        if d.calls.len() as u64 >= d.snap.max_tool_calls as u64 {
            let why = call_limit_exceeded_why(d.calls.len(), d.snap.max_tool_calls);
            d.halted = Some(HaltState {
                kind: "budget",
                why: why.clone(),
            });
            return envelope_err(caller, "budget", Some(&name), Some(false), &why);
        }
    }

    if name == TOOLS_LIST_NAME {
        // Reserved name: intercepted before the hooks gate by design
        // (§7) — it is not a tool call, so gate/audit/count don't apply.
        let list = caller
            .data()
            .defs
            .iter()
            .map(list_entry)
            .collect::<Vec<_>>();
        let payload = serde_json::to_string(&list).unwrap_or_else(|_| "[]".into());
        return envelope_or_trap(caller, true, &payload);
    }

    if name == BUDGET_NAME {
        // Reserved name (v2 review): read-only introspection — no hooks gate,
        // no audit, no call-quota cost (same class as `__tools_list`; a busy
        // budget-poll loop still dies on the wall-clock entry check above).
        // Fuel first: `get_fuel` wants the store, deadline/calls live in the
        // host data — sequence the borrows.
        let fuel_left = caller.get_fuel().ok();
        let d = caller.data();
        let used = d.calls.len() as u64;
        // Run-level token gate (`limits.max_total_tokens`, fleet-wide): null
        // when no gate is installed (0 = unlimited) — same nullable
        // convention as wall_left_ms / fuel_left. The guest ctx CLONES the
        // outer gate (Arc-backed `used`), so this reads the live counter.
        let tokens_left = d.ctx.budget.as_ref().map(|g| g.tokens_left());
        let (wall_left_ms, wall_bound_by) =
            d.deadline.map_or((None, serde_json::Value::Null), |w| {
                (
                    Some(w.at.saturating_duration_since(Instant::now()).as_millis() as u64),
                    if w.from_cfg {
                        json!("code.timeout_secs")
                    } else {
                        json!("run_deadline")
                    },
                )
            });
        let payload = json!({
            "calls_used": used,
            "max_tool_calls": d.snap.max_tool_calls,
            "calls_left": (d.snap.max_tool_calls as u64).saturating_sub(used),
            "wall_left_ms": wall_left_ms,
            "wall_bound_by": wall_bound_by,
            "fuel_left": fuel_left,
            "tokens_left": tokens_left,
        })
        .to_string();
        return envelope_or_trap(caller, true, &payload);
    }

    if name == INCLUDE_NAME || name == HASH_FILE_NAME {
        // Lib-family reserved names (MD-2): REAL gated host actions, not the
        // read-only introspection class above — routed to their own dispatch
        // with gate/audit/quota (see `lib_reserved_call`).
        return lib_reserved_call(caller, &name, &args);
    }

    if name == CALL_BATCH_NAME && caller.data().snap.call_batch {
        // The batch channel's reserved name (§4/LD-19): intercepted only when
        // `code.call_batch` is ON. Gate OFF deliberately falls through to the
        // ordinary Phase B below — an unknown-tool error, byte-identical to
        // the pre-batch binary (LD-19's 保留名直调钉住: gate-off direct calls
        // behave exactly like the feature was never compiled).
        return call_batch_dispatch(caller, &args);
    }

    // Phase B — dispatch: the same gate → run → after sequence the top-level
    // loop's inline pass runs (`execute_tool_call`'s exact order), hand-rolled
    // here because the deny/fs layer must sit BETWEEN the gate and the run —
    // a layer `execute_tool_call` deliberately doesn't know about. The store
    // borrow ends at the last read in this block; the emit sink is reached
    // through its raw pointer, which borrows nothing. The error CLASS travels
    // alongside the output so Phase C can classify the envelope without
    // re-deriving (or prefix-matching) where the failure came from.
    let started = Instant::now();
    let (in_len, in_hash) = hash_input(&args);
    let (output, in_hash_eff, err_class) = {
        let d = caller.data();
        // SAFETY: `emit` points at the sink borrowed in
        // `run_with_emit`; imports run synchronously inside the
        // `eval_code` call made from that same frame.
        let emit = unsafe { &mut *d.emit };
        emit(AgentEvent::ToolCall {
            name: name.clone(),
            input: args.clone(),
        });
        let decision = gate_tool_call(&d.ctx, &name, &args);
        let effective: Value = match &decision {
            ToolDecision::Modify(v) => v.clone(),
            _ => args.clone(),
        };
        // Dual-hash audit (§8.4): record the effective input only when a
        // Modify actually changed it — value equality, not re-serialization.
        let in_hash_eff = if effective != args {
            Some(hash_input(&effective).1)
        } else {
            None
        };
        let known = d.defs.iter().any(|t| t.name == name);
        let (output, class) = match decision {
            ToolDecision::Deny(mut reason) => {
                // Same output budget as any tool error: a host's deny reason
                // is model-visible is_error text and can be arbitrarily long.
                crate::tools::truncate(&mut reason, d.ctx.max_output_chars);
                let output = ToolOutput::error(reason);
                notify_tool_after(&d.ctx, &name, &args, &output, false);
                (output, "hooks")
            }
            _ => {
                // Cancel checkpoint between the hooks gate and execution
                // (external re-review P2): the §13-5 entry gate at the top of
                // host_call runs BEFORE before_tool, so a cancel landing
                // while the gate deliberated (an approval wait) would still
                // run the inner tool. Same tombstone vocabulary as the entry
                // gate, but no latch here — the guest's next call hits the
                // entry gate (or the epoch trap within ~10ms) and latches
                // interrupt+halted as usual. The call was already announced
                // above, so the pair completes: audited, event-paired,
                // "cancel" envelope (retryable=false — the whole execution is
                // stopping, not just this call).
                if ctx_cancelled(&d.ctx) {
                    let output = ToolOutput::error(CANCEL_HALTED_WHY.to_string());
                    notify_tool_after(&d.ctx, &name, &args, &output, false);
                    (output, "cancel")
                } else {
                    match intercept_file_paths(&name, &effective, &d.snap, &d.workdir_canon) {
                        // Rejected by the deny/fs layer: still an audited,
                        // event-paired call — but NOT a hooks decision, so
                        // `after_tool` does not fire (its `executed=false`
                        // is reserved for `before_tool` Deny and gate-time
                        // cancel, per the Hooks contract).
                        Some(err) => (ToolOutput::error(err), "denied"),
                        None => {
                            let output =
                                run_tool(&d.tools, &d.defs, &name, &effective, &d.ctx, emit);
                            notify_tool_after(&d.ctx, &name, &args, &output, true);
                            (output, if known { "tool" } else { "unknown_tool" })
                        }
                    }
                }
            }
        };
        (output, in_hash_eff, class)
    };
    emit_inner_result(caller, &name, &output);
    {
        let d = caller.data_mut();
        d.calls.push(CallTrace {
            name: name.clone(),
            ok: !output.is_error,
            ms: started.elapsed().as_millis(),
            in_len,
            in_hash,
            in_hash_eff,
        });
        fold_inner_usage(d, &output);
    }

    // Phase C — hand the envelope back (needs the mutable caller again).
    // Audit nuance (§8.4): an image-bearing call DID run — side effects
    // happened, its event pair reports success, the trace keeps ok:true.
    // The error envelope below is run_code's transport-level inability to
    // carry an image into the sandbox, not a tool failure.
    if output.image.is_some() {
        envelope_err(
            caller,
            "image",
            Some(&name),
            Some(false),
            "run_code: this tool returned an image, which run_code cannot carry — \
             call read_file on the path instead, or use load_image at the top level",
        )
    } else if output.is_error {
        let retryable = if err_class == "tool" {
            None
        } else {
            Some(false)
        };
        envelope_err(caller, err_class, Some(&name), retryable, &output.content)
    } else {
        envelope_or_trap(caller, true, &output.content)
    }
}

/// Emit the inner call's ToolResult event (H1: emission belongs to the
/// caller — the outer loop assembles results only for its own calls).
fn emit_inner_result(caller: &mut Caller<'_, HostCtx>, name: &str, output: &ToolOutput) {
    let d = caller.data();
    // SAFETY: same frame-limited alias as in `host_call_tool`.
    let emit = unsafe { &mut *d.emit };
    emit(AgentEvent::ToolResult {
        name: name.to_string(),
        is_error: output.is_error,
        content: output.content.clone(),
    });
}

/// Saturating usage fold for inner-call billing (same shape as the core's).
/// One `tools.list()` entry: name + description capped at 200 chars (§2 —
/// 50 tools with long descriptions would bloat the guest's memory for no
/// value; the model only needs to recognize names).
fn list_entry(def: &ToolDef) -> Value {
    let head: String = def.description.chars().take(200).collect();
    json!({ "name": def.name, "description": head })
}

// ── deny_paths / fs_write_dirs interception (design §7) ─────────────────────
//
// Positioning (D13): this guards the model MISREADING a path ("back up my env
// file" landing on `.env`), not a hostile guest — shell passes `cat .env`
// straight through when it's whitelisted, so this is not a security boundary.
// The hostile-guest remedies are restrict_to_workdir, hooks Deny, or dropping
// shell from the whitelist (A-8 pins the pass-through).

/// The file tools whose `path` argument the layer judges. Everything else
/// (shell, MCP, task, …) is out of scope by design. `copy_bytes` is judged
/// separately (two paths, [`intercept_copy`]) but lives in the same layer.
const FILE_TOOLS: [&str; 4] = ["read_file", "write_file", "edit_file", "list_dir"];
const WRITE_TOOLS: [&str; 2] = ["write_file", "edit_file"];

/// Judge one inner call. `Some(err)` = rejected (the error text becomes the
/// guest-visible exception); `None` = pass through to the tool.
fn intercept_file_paths(
    name: &str,
    effective: &Value,
    snap: &CodeConfig,
    workdir_canon: &Path,
) -> Option<String> {
    if name == "copy_bytes" {
        return intercept_copy(effective, snap, workdir_canon);
    }
    if !FILE_TOOLS.contains(&name) {
        return None; // A-8: shell & friends are deliberately not intercepted
    }
    let Some(path) = effective.get("path").and_then(Value::as_str) else {
        return None; // the tool itself reports a missing/ill-typed path
    };
    let is_write = WRITE_TOOLS.contains(&name);

    // 中-1 (whole-chain, per sec-review M-1): writes reject a symlink in ANY
    // component under the workdir, not just the final one. A symlinked
    // component makes the write's real destination unknowable from the
    // spelling the model chose — `out -> /tmp/pwn` means `out/x` lands
    // outside the workdir even though the lexical form says otherwise; a
    // dangling `ln -s .env bak` lands the write on `.env` while the model
    // chose "bak". With NO symlink component under the canonical workdir the
    // lexical form IS the resolved form, so the policy judgment below is
    // sound. (TOCTOU between check and write stays the documented best-effort
    // boundary, same as confined_to.)
    if is_write && write_traverses_symlink(workdir_canon, path) {
        return Some(format!(
            "run_code: refusing to write through a symlink ('{path}') — \
             write the real path instead"
        ));
    }

    // Normalize to a workdir-relative path (中-2): independent of
    // `confined_to` (whose semantics bind to restrict_to_workdir), so this
    // layer holds with either sandbox mode. `./x`, `a/../x`, absolute-inside
    // spellings all collapse to one comparable form.
    let rel = match workdir_relative(workdir_canon, path) {
        Some(r) => r,
        None => {
            if is_write {
                // Guest writes stay inside the workdir, period — the
                // fs_write_dirs model is a permission LIST bounded by the
                // workdir ("empty = the whole workdir is writable"), not a
                // mirror of restrict_to_workdir's off mode.
                return Some(format!(
                    "run_code: guest writes are limited to the working \
                     directory ('{path}' is outside)"
                ));
            }
            // Reads outside workdir: deny_paths cannot see them — the
            // documented boundary (中-2). The remedy for hostile guests is
            // restrict_to_workdir or a hooks Deny.
            return None;
        }
    };

    if deny_match(&snap.deny_paths, &rel) {
        return Some(format!(
            "run_code: path '{path}' matches a code.deny_paths pattern"
        ));
    }
    if is_write && !snap.fs_write_dirs.is_empty() && !under_write_dir(&snap.fs_write_dirs, &rel) {
        return Some(format!(
            "run_code: write target '{path}' is outside code.fs_write_dirs"
        ));
    }
    None
}

/// The `copy_bytes` arm of [`intercept_file_paths`] (v2 review): a copy is a
/// read of `src` AND a write of `dst`, so each side gets its own policy —
/// `src` under READ rules (deny_paths only; outside-workdir reads stay the
/// documented 中-2 boundary), `dst` under WRITE rules (symlink-traversal
/// refusal, workdir confinement, deny_paths, fs_write_dirs — same order as
/// the main arm). Missing/ill-typed fields pass through as `None` so the
/// tool itself reports them, mirroring the main arm.
fn intercept_copy(effective: &Value, snap: &CodeConfig, workdir_canon: &Path) -> Option<String> {
    let src = effective.get("src").and_then(Value::as_str)?;
    let dst = effective.get("dst").and_then(Value::as_str)?;
    // dst first — a refused write should fail before the read is even judged.
    if write_traverses_symlink(workdir_canon, dst) {
        return Some(format!(
            "run_code: refusing to write through a symlink ('{dst}') — \
             write the real path instead"
        ));
    }
    let rel = match workdir_relative(workdir_canon, dst) {
        Some(r) => r,
        None => {
            return Some(format!(
                "run_code: guest writes are limited to the working \
                 directory ('{dst}' is outside)"
            ))
        }
    };
    if deny_match(&snap.deny_paths, &rel) {
        return Some(format!(
            "run_code: path '{dst}' matches a code.deny_paths pattern"
        ));
    }
    if !snap.fs_write_dirs.is_empty() && !under_write_dir(&snap.fs_write_dirs, &rel) {
        return Some(format!(
            "run_code: write target '{dst}' is outside code.fs_write_dirs"
        ));
    }
    if let Some(rel) = workdir_relative(workdir_canon, src) {
        if deny_match(&snap.deny_paths, &rel) {
            return Some(format!(
                "run_code: path '{src}' matches a code.deny_paths pattern"
            ));
        }
    }
    None
}

/// The guest-only `copy_bytes` tool (v2 review): byte-exact file copy that
/// never round-trips content through a JS string — the model decides src and
/// dst, the host streams the bytes (binary, invalid-UTF-8, and oversized-
/// for-guest-memory files all survive; the raw+content_b64 recipe in the
/// skill is the manual version). Registered ONLY in the sandbox's guest
/// registry — never the agent's top-level list — so it is a run_code
/// primitive, not a capability the plain (non-code) route gains. Policy is
/// judged in [`intercept_copy`] before this runs; this body only re-resolves
/// paths (with the tool-side restrict_to_workdir sandbox, mirroring
/// read_file/write_file) and streams.
struct CopyBytesTool;

impl Tool for CopyBytesTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "copy_bytes".into(),
            description: "字节保真复制文件（host 流式搬运，内容不经过 JS 字符串）：\
                          二进制/非 UTF-8/大文件均逐字节一致。适合导出、备份、迁移\
                          等需要保持原文的复制；要改内容不要用它。"
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "src": {"type": "string", "description": "源文件路径（相对 workdir）"},
                    "dst": {"type": "string", "description": "目标路径（须在 workdir 内）"}
                },
                "required": ["src", "dst"]
            }),
        }
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let src = input
            .get("src")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("copy_bytes: missing string field 'src'"))?;
        let dst = input
            .get("dst")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("copy_bytes: missing string field 'dst'"))?;
        let resolve = |p: &str, label: &str| {
            let full = absolutize(&ctx.workdir, p);
            if ctx.restrict_to_workdir {
                crate::tools::confined_to(&ctx.workdir, &full, label)
            } else {
                Ok(full)
            }
        };
        let (s, d) = match (resolve(src, "copy source"), resolve(dst, "copy target")) {
            (Ok(s), Ok(d)) => (s, d),
            (Err(e), _) | (_, Err(e)) => {
                return Ok(ToolOutput::error(format!("copy_bytes: {e:#}")))
            }
        };
        match std::fs::copy(&s, &d) {
            Ok(n) => Ok(ToolOutput::text(
                json!({"copied_bytes": n, "src": src, "dst": dst}).to_string(),
            )),
            Err(e) => Ok(ToolOutput::error(format!(
                "copy_bytes: {} -> {}: {e}",
                s.display(),
                d.display()
            ))),
        }
    }
}

/// Join a guest path onto the workdir unless absolute (the same resolution
/// the file tools themselves apply — the layer must judge the path the tool
/// would actually touch).
fn absolutize(workdir: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        workdir.join(p)
    }
}

/// Normalize to the workdir-relative, `/`-separated form. Returns `None`
/// when the path resolves outside the workdir (deny_paths "not applicable")
/// or is too long/deep to be a real path (see [`MAX_REL_PATH_BYTES`] —
/// reads then fall through to the tool and fail naturally, writes are
/// denied). Canonicalize first (resolving symlinks that EXIST); on failure
/// (target not there yet — the normal write case) resolve the deepest
/// EXISTING ancestor of the ORIGINAL spelling and append the remainder
/// ([`resolve_via_existing_ancestor`]) — kernel-truth, unlike the previous
/// pure-lexical fallback, which collapsed `link/..` away on paper and judged
/// `/outside/../<workdir>/link/../x` as INSIDE while the kernel wrote it
/// outside (external re-review P1).
fn workdir_relative(workdir_canon: &Path, path: &str) -> Option<String> {
    let full = absolutize(workdir_canon, path);
    let canon = match full.canonicalize() {
        Ok(c) => c,
        Err(_) => {
            // Payload guard BEFORE the ancestor walk: over-long spellings end
            // up denied (writes) / pass-through-then-ENOENT (reads) either
            // way — bound the one-canonicalize-per-level walk first
            // (sec-review M-2 shape).
            if path.len() > MAX_REL_PATH_BYTES {
                return None;
            }
            resolve_via_existing_ancestor(&full).unwrap_or_else(|| lexical_normalize(&full))
        }
    };
    let rel = canon.strip_prefix(workdir_canon).ok()?;
    let rel = rel.to_string_lossy().replace('\\', "/");
    if rel.len() > MAX_REL_PATH_BYTES || rel.split('/').count() > MAX_REL_SEGMENTS {
        return None;
    }
    Some(rel)
}

/// Kernel-truth resolution for a path whose tail does not exist yet (the
/// normal write case): canonicalize the deepest EXISTING ancestor of the
/// ORIGINAL spelling — resolving every symlink and `..` on the way, exactly
/// as the kernel will at write time — then append the not-yet-existing
/// remainder lexically. The remainder can hold no `..`: `file_name()` of a
/// `..`-terminated path is `None`, and that case bails to the caller's
/// lexical fallback — preserving the old behavior for `missing/../file`
/// spellings, which the tool layer answers (ENOENT or parent creation),
/// never a policy refusal.
///
/// `None` also past [`MAX_REL_SEGMENTS`] stripped components: deeper tails
/// are denied by `workdir_relative`'s segment check regardless, so the walk
/// stops before a guest-controlled depth turns it into a syscall-per-level
/// stall.
fn resolve_via_existing_ancestor(full: &Path) -> Option<PathBuf> {
    let mut ancestor: &Path = full;
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(mut resolved) => {
                for comp in rest.iter().rev() {
                    resolved.push(comp);
                }
                return Some(resolved);
            }
            Err(_) => {
                if rest.len() >= MAX_REL_SEGMENTS {
                    return None;
                }
                rest.push(ancestor.file_name()?.to_os_string());
                ancestor = ancestor.parent()?;
            }
        }
    }
}

/// No real path resolves longer/deeper than this: PATH_MAX (4096) bounds
/// every syscall path argument, so past these bounds the string is not a
/// filename but a payload (sec-review M-2: a guest-controlled ~500k-segment
/// path used to drive [`glob_match`]'s recursive `**` arm into a host stack
/// overflow — an uncatchable abort no fuel/mem limiter sees).
const MAX_REL_PATH_BYTES: usize = 4096;
const MAX_REL_SEGMENTS: usize = 256;

/// Does the write target traverse a symlink component under the workdir?
/// Iterative — depth is bounded by the path guards and never guest-controlled
/// recursion. Two-stage judgment:
///
/// 1. Lexical screen: fold the spelling and bail early when it collapses
///    outside the workdir (the outside-workdir rule rejects those right
///    after — no walk needed).
/// 2. Symlink walk on the ORIGINAL spelling, component by component
///    (external re-review P1): the kernel applies `..` only AFTER resolving
///    the component in front of it — `link/../x` resolves THROUGH `link`,
///    and the tool writes the original spelling, so that is what the
///    kernel actually does at write time. The old code folded `..` FIRST,
///    which erases `link` from the walk and shows the policy layer a clean
///    path while the real write escapes. Walking original order —
///    push-and-check on Normal, pop on ParentDir — rejects a symlink in any
///    original position; every component that survives its check is a real
///    directory, so a lexical pop matches the kernel's resolution.
///
/// Absolute spellings walk ANCHORED AT THE FILESYSTEM ROOT (second external
/// P1): the old code `strip_prefix`'d the original absolute spelling and gave
/// up when its literal front differed from the canonical workdir — but
/// `/outside/../<workdir>/link/../x` collapses back inside (passing the
/// screen above) while never spelling the workdir as its literal prefix, so
/// the `link` it traverses went unchecked. Root-anchored walking covers every
/// spelling the screen admits: a component above the workdir in such a
/// spelling is either a real directory (lexical pop == kernel pop) or itself
/// a symlink — a resolution we cannot reason about lexically, which is
/// exactly what this layer refuses.
fn write_traverses_symlink(workdir_canon: &Path, path: &str) -> bool {
    let full = lexical_normalize(&absolutize(workdir_canon, path));
    if full.strip_prefix(workdir_canon).is_err() {
        return false;
    }
    // Payload paths (sec-review M-2): the walk below pays one symlink_metadata
    // per segment — µs-scale on unix, ~100µs+ on Windows filter drivers (60k
    // segments measured 12s there). Over-limit spellings are denied
    // fail-closed by workdir_relative's length check right after regardless,
    // so skipping the walk only saves the doomed syscalls, never flips a
    // decision. Guard the ORIGINAL spelling: it is what drives the walk.
    if path.len() > MAX_REL_PATH_BYTES {
        return false;
    }
    let orig = Path::new(path);
    if orig.components().count() > MAX_REL_SEGMENTS {
        return false;
    }
    use std::path::Component;
    // Relative spellings anchor at the canonical workdir; absolute ones at
    // the filesystem root (see the fn doc — a literal strip_prefix of the
    // original spelling misses `x/../<workdir>/…` spellings entirely).
    let mut cur = if orig.is_absolute() {
        PathBuf::new()
    } else {
        workdir_canon.to_path_buf()
    };
    for comp in orig.components() {
        match comp {
            Component::Normal(c) => {
                cur.push(c);
                if std::fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink()) {
                    return true;
                }
            }
            Component::ParentDir => {
                cur.pop();
            }
            Component::CurDir => {}
            // Rebuild the absolute anchor: the Windows prefix ("C:") then the
            // root — pushing MAIN_SEPARATOR onto the empty/prefix-only buffer
            // yields "/" / "C:\". Neither the root nor a canonical workdir
            // prefix is itself a symlink, so not checking them loses nothing.
            Component::Prefix(p) => cur.push(p.as_os_str()),
            Component::RootDir => cur.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
        }
    }
    false
}

/// Lexical `.`/`..` collapse WITHOUT symlink resolution — used for the
/// escape-detection screen and as `workdir_relative`'s fallback-of-last-
///-resort (an ancestor chain whose not-yet-existing component precedes a
/// `..`, where the real write fails ENOENT regardless). `..` past the root
/// saturates (parts simply run empty — a no-op), matching the
/// escape-detection above (a path that lexical-collapses outside the
/// workdir fails `strip_prefix` → outside).
///
/// NOT `out.push(c.as_os_str())` per component: on Windows, per-component
/// `PathBuf::push` fed from a verbatim-joined long path costs ~1.5ms EACH
/// (60k segments measured 92s, while components() iteration and any
/// whole-string build are µs — Windows E2E finding, sec-review M-2 shape).
/// Collect, collapse, then assemble with amortized-O(1) OsString pushes:
/// linear on every OS, identical collapse semantics.
fn lexical_normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut head = PathBuf::new(); // prefix + root, kept verbatim as-is
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => head.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(name) => parts.push(name.to_os_string()),
        }
    }
    let sep: &std::ffi::OsStr = std::path::MAIN_SEPARATOR_STR.as_ref();
    let mut out = head.into_os_string();
    for part in parts {
        // `PathBuf::push` inserts a separator only between existing content
        // and the part — an empty base (relative paths: head is empty) must
        // not grow a leading one, or "out" becomes "\out".
        if !out.is_empty() && !ends_with_sep(&out) {
            out.push(sep);
        }
        out.push(&part);
    }
    PathBuf::from(out)
}

/// The rightmost-byte separator check `PathBuf::push` performs, byte-level:
/// exact because UTF-8/WTF-8 never embeds 0x2F or 0x5C inside a multi-byte
/// sequence.
fn ends_with_sep(s: &std::ffi::OsStr) -> bool {
    match s.as_encoded_bytes().last() {
        Some(b'/') => true,
        #[cfg(windows)]
        Some(b'\\') => true,
        _ => false,
    }
}

/// Does any deny pattern match the workdir-relative path?
fn deny_match(patterns: &[String], rel: &str) -> bool {
    let folded: Vec<String> = fold_case(rel).split('/').map(String::from).collect();
    let path: Vec<&str> = folded.iter().map(String::as_str).collect();
    patterns.iter().any(|p| {
        let folded_pat = fold_case(p);
        let pat: Vec<&str> = folded_pat.split('/').collect();
        glob_match(&pat, &path)
    })
}

/// Containment against fs_write_dirs: `rel == dir` or `rel` nested under it.
/// Entries are lexical dir paths (not globs), cleaned with the same lexical
/// pass as guest paths — `./out`, `out/`, `out//x` all behave like their
/// clean spellings (an entry that cleans to nothing matches nothing).
fn under_write_dir(dirs: &[String], rel: &str) -> bool {
    let rel = fold_case(rel);
    dirs.iter().any(|d| {
        let cleaned = lexical_normalize(Path::new(d));
        let d = fold_case(&cleaned.to_string_lossy());
        !d.is_empty() && (rel == d || rel.starts_with(&format!("{d}/")))
    })
}

/// Segment-wise glob match: `*` stays inside a segment, a whole `**` segment
/// crosses any number of segments (zero included — `**/.env*` also hits the
/// workdir root), and a pattern with no `/` matches only the FIRST segment
/// (`.env*` does not hit `a/.env`). Classic recursive matcher over segments.
fn glob_match(pat: &[&str], path: &[&str]) -> bool {
    match (pat.split_first(), path.split_first()) {
        (Some((&"**", rest_p)), _) => {
            glob_match(rest_p, path)
                || path
                    .split_first()
                    .is_some_and(|(_, rest_s)| glob_match(pat, rest_s))
        }
        (Some((p, rest_p)), Some((s, rest_s))) => seg_match(p, s) && glob_match(rest_p, rest_s),
        (None, None) => true,
        _ => false,
    }
}

/// One-segment wildcard match (`*` = any run of chars, no `/` crossing —
/// there is no `/` left inside a segment to cross).
fn seg_match(pat: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    // DP over (pattern idx, subject idx) — small strings, no backtracking blowup.
    let mut dp = vec![vec![false; s.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for i in 1..=p.len() {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=p.len() {
        for j in 1..=s.len() {
            dp[i][j] = if p[i - 1] == '*' {
                dp[i - 1][j] || dp[i][j - 1]
            } else {
                dp[i - 1][j - 1] && p[i - 1] == s[j - 1]
            };
        }
    }
    dp[p.len()][s.len()]
}

/// Case-fold for path comparison. macOS (APFS default) and Windows volumes
/// are case-insensitive — `.ENV` must hit `.env` there. Linux keeps exact
/// case (over-matching would deny files that aren't the named secret).
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn fold_case(s: &str) -> String {
    s.to_lowercase()
}
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn fold_case(s: &str) -> String {
    s.to_string()
}

/// Compact-JSON length + truncated sha256 (first 16 bytes, hex) for the audit
/// trace — the 128-bit prefix is beyond any practical collision effort, and
/// the full plaintext already lives in the event stream (§8.4's 明文口径:
/// "not plaintext" applies to result.json only, which these hashes harden).
fn hash_input(v: &Value) -> (usize, String) {
    let s = serde_json::to_string(v).unwrap_or_default();
    let digest = sha2::Sha256::digest(s.as_bytes());
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    (s.len(), format!("sha256:{hex}"))
}

/// The audit `meta` for a finished execution (§8.4): per-call trace + count
/// against limit + fuel actually consumed. `truncated`/`logs_dropped` land
/// only when true / non-zero (sparse — the common case stays compact).
/// Assemble the final content — value half + collected console logs — under
/// `cap` characters (D3): the RETURN VALUE outranks the logs. Fit whole log
/// lines into whatever budget the value leaves (separator included); only a
/// value that overflows ALONE gets tail-truncated (logs then drop entirely).
/// Returns `(content, truncated)`.
///
/// The value half is passed BEFORE the logs are merged — an earlier version
/// truncated the merged string mid-line (correctness-review M-1: the
/// line-fitting loop below it was dead code, the cap landed inside a log
/// line).
fn assemble_with_logs(mut value: String, logs: &[String], cap: usize) -> (String, bool) {
    const SEP: &str = "\n\n# console\n";
    if logs.is_empty() {
        if value.chars().count() > cap {
            crate::tools::truncate(&mut value, cap);
            return (value, true);
        }
        return (value, false);
    }
    // +1 per line = the join's '\n's (n lines need n-1; charging n keeps the
    // fit conservative by at most one char and matches the host log cap's
    // per-line accounting).
    let logs_chars: usize = logs.iter().map(|l| l.chars().count() + 1).sum();
    let sep = SEP.chars().count();
    let value_len = value.chars().count();
    if value_len + sep + logs_chars.saturating_sub(1) <= cap {
        value.push_str(SEP);
        value.push_str(&logs.join("\n"));
        return (value, false);
    }
    if value_len > cap {
        // Value alone busts the budget: value wins, logs drop entirely.
        crate::tools::truncate(&mut value, cap);
        return (value, true);
    }
    let mut budget = cap.saturating_sub(value_len).saturating_sub(sep);
    let mut included: Vec<&str> = Vec::new();
    for line in logs {
        let need = line.chars().count() + 1;
        if need > budget {
            break;
        }
        budget -= need;
        included.push(line.as_str());
    }
    if included.is_empty() {
        // No room for even one line plus the separator — value rides alone.
        return (value, true);
    }
    value.push_str(SEP);
    value.push_str(&included.join("\n"));
    (value, true)
}

fn audit_meta(
    calls: &[CallTrace],
    snap: &CodeConfig,
    fuel_used: u64,
    truncated: bool,
    logs_dropped: u64,
) -> Value {
    let entries: Vec<Value> = calls
        .iter()
        .map(|c| {
            let mut e = json!({
                "name": c.name,
                "ok": c.ok,
                "ms": c.ms,
                "in_len": c.in_len,
                "in_hash": c.in_hash,
            });
            if let Some(h) = &c.in_hash_eff {
                e["in_hash_eff"] = json!(h);
            }
            e
        })
        .collect();
    let mut meta = json!({
        "calls": entries,
        "call_count": calls.len(),
        "call_limit": snap.max_tool_calls,
        "fuel_used": fuel_used,
    });
    if truncated {
        meta["truncated"] = json!(true);
    }
    if logs_dropped > 0 {
        meta["logs_dropped"] = json!(logs_dropped);
    }
    meta
}

/// Write the envelope into the guest, or fabricate a same-shaped error
/// envelope in host memory if even that fails (allocation trapped / OOB) —
/// the guest contract requires SOME (ptr,len) back from every call.
fn envelope_or_trap(caller: &mut Caller<'_, HostCtx>, is_ok: bool, payload: &str) -> i64 {
    let bytes = if is_ok {
        json!({ "__ok": true, "content": payload }).to_string()
    } else {
        json!({ "__ok": false, "error": payload }).to_string()
    };
    match write_guest(caller, bytes.as_bytes()) {
        Ok(packed) => packed,
        Err(_) => {
            // alloc trapped (fuel/memory exhausted mid-call) — mirror the
            // exhaustion as the payload so the guest exits with the right
            // diagnosis instead of a bogus envelope.
            let fallback = json!({
                "__ok": false,
                "error": format!(
                    "run_code: host could not return the tool result ({payload})"
                )
            })
            .to_string();
            // guest is unserviced; eval will trap momentarily
            write_guest(caller, fallback.as_bytes()).unwrap_or_default()
        }
    }
}

/// [`envelope_or_trap`] with a HOST-KNOWN failure class (v2 review: batch
/// code should branch on why a call failed, not parse message strings).
/// Adds `kind` / `tool` / `retryable` to the envelope; the glue attaches
/// them to the thrown Error (`e.kind`, `e.tool`, `e.retryable`) and older
/// string-only reasoning keeps working via `e.message`. Vocabulary:
/// "denied" (deny_paths / fs_write_dirs / symlink / outside-workdir — and
/// the lib channel's policy layer, incl. the code.include=false refusal —
/// definitive, retrying the same call cannot help), "hooks" (before_tool
/// Deny — definitive), "budget" (call limit latched; terminal for this
/// execution, resets on the next run_code), "deadline" (wall clock;
/// terminal), "cancel" (the host's run-level cancel flag; terminal — the
/// whole execution is stopping, not just this call), "unknown_tool"
/// (definitive), "image" (the call RAN — the result just cannot be carried
/// into the sandbox), "host" (envelope / pointer machinery), "lib" (the
/// include family's verification chain — not-registered / hash mismatch /
/// malformed manifest / read-cap; definitive). Plain tool
/// failures use "tool" and deliberately
/// OMIT `retryable`: the host cannot classify a tool's own error, and both
/// hard values mislead (false suppresses legitimate retries, true retries
/// deterministic failures) — the model keeps judging from the message.
fn envelope_err(
    caller: &mut Caller<'_, HostCtx>,
    kind: &'static str,
    tool: Option<&str>,
    retryable: Option<bool>,
    msg: &str,
) -> i64 {
    let mut env = json!({ "__ok": false, "error": msg, "kind": kind });
    if let Some(t) = tool {
        env["tool"] = json!(t);
    }
    if let Some(r) = retryable {
        env["retryable"] = json!(r);
    }
    match write_guest(caller, env.to_string().as_bytes()) {
        Ok(packed) => packed,
        Err(_) => envelope_or_trap(caller, false, msg),
    }
}

/// The guest-side [`ToolCtx`]: the outer call's ctx with the `__code__`
/// marker. Hooks Arc-clone along (a tool denied at the top level stays denied
/// inside the sandbox — no bypass), the budget counter and deadline are
/// shared, same as the `task` child ctx.
///
/// `max_output_chars` is deliberately NOT inherited: that knob is a
/// DISPLAY-scale budget for what enters the MODEL's context, while guest tool
/// outputs land in JS heap memory (bounded by `code.mem_mb` via the store
/// limiter) and never enter the context at all — the design's own example
/// (`SELECT … LIMIT 5000` feeding a JS aggregation) assumes data-scale
/// outputs. Real-journey A/B found the inheritance defeating the feature's
/// core value: `tools.call("read_file")` inside the sandbox returned one
/// 1000-char page no matter the requested limit, so the model fell back to 17
/// manual read_file pages through its own context and burned the run's whole
/// token budget re-reading its own history. Char unit stays (so truncate() /
/// the read_file byte-window math keep working); only the scale changes.
const GUEST_OUTPUT_CHARS: usize = 2_000_000;

/// Built-in skill carrying run_code's full usage guide (v2 P2). The tool
/// description stays slim — identity, environment, the CLOSED helper list and
/// a pointer here — because it rides in EVERY request's tool schema; the deep
/// contracts (read_file's three modes, the byte-exact copy recipe, examples)
/// move to this skill, loaded on demand via `read_skill` as a compressible
/// tool_result instead of permanent schema. Shipped as a sibling .md file so
/// it diffs like a doc; `assemble` embeds it via `skills::embedded_entry`
/// when (and only when) the tool is enabled, and a disk skill named
/// `run-code` overrides it (the shipped guide is never a cage).
pub(crate) const RUN_CODE_SKILL_MD: &str = include_str!("run_code_skill.md");

/// The skill body for one assembly (MD-13, M2): gated sections are spliced at
/// assembly time — `code.include` keeps/strips the `BEGIN/END LIB SECTION`
/// block, `code.call_batch` the `BEGIN/END BATCH SECTION` block (LD-15);
/// with every gate off the result is the pre-include/pre-batch contract.
/// Assembly-time decision (the `skill_injection` snapshot precedent): a
/// later `set_code_config` does not re-splice an already-built skill index.
/// Marker discipline: exactly one BEGIN and one END line per section, BEGIN
/// before END; a malformed marker set is a hard error at assembly (fail
/// loud, never silently ship a half-section).
pub fn run_code_skill_md(include: bool, call_batch: bool) -> String {
    /// Strip one marked section (fail loud on a malformed marker set).
    fn strip_once(src: &str, begin: &str, end: &str, what: &str) -> String {
        let (b, e) = match (src.find(begin), src.find(end)) {
            (Some(b), Some(e)) if b < e => (b, e + end.len()),
            _ => panic!("run_code_skill.md: {what} markers malformed"),
        };
        let mut out = String::with_capacity(src.len());
        out.push_str(&src[..b]);
        out.push_str(src[e..].trim_start_matches('\n'));
        out
    }
    let mut out = RUN_CODE_SKILL_MD.to_string();
    if !include {
        out = strip_once(
            &out,
            "<!-- BEGIN LIB SECTION",
            "<!-- END LIB SECTION -->",
            "LIB SECTION",
        );
    }
    if !call_batch {
        out = strip_once(
            &out,
            "<!-- BEGIN BATCH SECTION",
            "<!-- END BATCH SECTION -->",
            "BATCH SECTION",
        );
    }
    out
}

/// Guest prelude (v2 P1): a tiny, CLOSED set of I/O helpers so model-generated
/// code expresses intent instead of re-implementing paging loops and
/// line-prefix stripping — the top iteration sources in the runnability
/// sample (task1-flash ×3 was exactly a text-mode parsing contract). Three
/// helpers only: `readAll` (paged full-text read via format:"json"),
/// `readLines` (the same as an array), `writeVerified` (write then byte-exact
/// read-back compare — the no-shell byte-fidelity path). Flattened to ONE
/// physical line by [`prelude_line`] before eval: quickjs reports error line
/// numbers relative to the eval'd source, so a multi-line prelude would shift
/// every user stack frame by its length; flattened, the shift is a constant
/// +1. Discipline for additions: every new helper is a new hallucination
/// surface ("the model assumes a helper that doesn't exist") — the
/// LLM-visible description names exactly these three, keep both in lockstep.
const PRELUDE_SRC: &str = r#"
var __TA_B64T = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
function __ta_utf8(s) {
    var u = unescape(encodeURIComponent(s)), a = new Uint8Array(u.length), i;
    for (i = 0; i < u.length; i++) a[i] = u.charCodeAt(i);
    return a;
}
function __ta_b64(a) {
    var out = [], i, n, T = __TA_B64T;
    for (i = 0; i + 2 < a.length; i += 3) {
        n = (a[i] << 16) | (a[i + 1] << 8) | a[i + 2];
        out.push(T[(n >> 18) & 63], T[(n >> 12) & 63], T[(n >> 6) & 63], T[n & 63]);
    }
    var r = a.length - i;
    if (r === 1) { n = a[i] << 16; out.push(T[(n >> 18) & 63], T[(n >> 12) & 63], "=="); }
    else if (r === 2) { n = (a[i] << 16) | (a[i + 1] << 8); out.push(T[(n >> 18) & 63], T[(n >> 12) & 63], T[(n >> 6) & 63], "="); }
    return out.join("");
}
function readAll(path, opts) {
    var out = [], off = 1, lim = (opts && opts.pageLines) || 20000;
    for (;;) {
        var r = tools.call("read_file", {path: path, offset: off, limit: lim, format: "json"});
        if (!r || typeof r !== "object" || typeof r.content !== "string") throw new Error("readAll: unexpected read_file output");
        if (r.content) out.push(r.content);
        // Pages carry their own line terminators (json content preserves
        // CRLF and the final newline), so concatenation is byte-exact — a
        // joining "\n" would inject a blank line at every page boundary.
        if (!r.truncated) return out.join("");
        if (r.next_offset == null) throw new Error("readAll: a single line exceeds the page budget at line " + off + " — page with a smaller limit or use raw reads");
        off = r.next_offset;
    }
}
function readLines(path) {
    return readAll(path).split("\n");
}
function writeVerified(path, content) {
    tools.call("write_file", {path: path, content: content});
    var bytes = __ta_utf8(content), expect = __ta_b64(bytes), off = 0;
    var CH = 1399995;
    for (;;) {
        var r = tools.call("read_file", {path: path, raw: true, offset: off, limit: CH});
        if (!r || typeof r !== "object" || typeof r.content_b64 !== "string") throw new Error("writeVerified: unexpected read_file output");
        var seg = expect.substr((off / 3) * 4, r.content_b64.length);
        if (r.content_b64 !== seg) throw new Error("writeVerified: read-back mismatch in " + path + " at byte " + off + " — the file on disk differs from what was written");
        if (!r.truncated) {
            // EOF alone is NOT success: a SHORTER file (a hooks rewrite, a
            // partial write) reads back as a matching prefix, so the slice
            // compare above passes — only the total-length check catches it.
            if (off + r.bytes_read !== bytes.length) throw new Error("writeVerified: length mismatch in " + path + " — expected " + bytes.length + " bytes on disk, read back " + (off + r.bytes_read));
            return "verified " + content.length + " chars to " + path;
        }
        off += r.bytes_read;
    }
}
tools.budget = function () { return tools.call("__budget", {}); };
"#;

/// The lib half of the prelude (code_memory_design §3.1, M1) — appended by
/// [`prelude_line`] only when `code.include` is on (MD-13: gate off = the
/// helpers do not exist, `typeof include === "undefined"`). Same closed-list
/// discipline as above: the gate-ON description names exactly these two, in
/// lockstep with the 【库】 overlay.
///
/// `include` is idempotent per path spelling and cycle-cut: the marker is
/// set BEFORE the indirect eval, so `a → b → a` loads `a` once and finishes;
/// a marker left behind on a throwing eval would poison the session, so it
/// is deleted on throw (a retry re-attempts, and fails again, honestly).
/// `(0, eval)` evaluates in global scope — the lib's declarations land on
/// the global object, visible to the calling program and to later includes.
///
/// `saveLib` = the MD-10 write path: `write_file` (full write policy
/// applies) → `__hash_file` (host hashes the delivered bytes AND registers
/// the session admission) → INDEX upsert (st always `draft`; existing
/// `needs`/`ver` preserved, ver=1/needs=- on create). The manifest write
/// goes through the ordinary file tools — the host only ever PARSES INDEX
/// (see [`parse_lib_index`]). The pre-existing `writeVerified` (byte-fidelity
/// write, v2 P1) is a different helper and stays untouched: the design doc's
/// "writeVerified" naming collided with it, the M1 helper is `saveLib`
/// (revision note in docs/code_memory_design.md).
const PRELUDE_LIB_SRC: &str = r#"
var __ta_libs = Object.create(null);
function include(path) {
    if (__ta_libs[path]) return undefined;
    var src = tools.call("__include", {path: path});
    __ta_libs[path] = true;
    try { (0, eval)(src); }
    catch (e) { delete __ta_libs[path]; throw e; }
    return undefined;
}
function saveLib(path, src) {
    path = String(path);
    if (path.indexOf("lib/") !== 0) throw new Error("saveLib: path must start with lib/ (got " + path + ")");
    // lib/INDEX.md machine lines are whitespace-tokenized on the host — a
    // whitespace path would register fine here and poison EVERY later
    // include with a malformed-index error. Reject before any write.
    if (/\s/.test(path)) throw new Error("saveLib: path must not contain whitespace — INDEX.md lines are whitespace-tokenized (got " + path + ")");
    tools.call("write_file", {path: path, content: src});
    var h = tools.call("__hash_file", {path: path});
    var rel = path.slice(4), tok = "path=" + rel;
    var text = "";
    try { text = readAll("lib/INDEX.md"); } catch (e) {}
    var lines = text ? text.split("\n") : [], out = [], hit = false, needs = "-", ver = "1", i, j, seg;
    for (i = 0; i < lines.length; i++) {
        seg = lines[i].split(" ");
        if (lines[i].indexOf("ta-lib:") !== -1 && seg.indexOf(tok) !== -1) {
            for (j = 0; j < seg.length; j++) {
                if (seg[j].indexOf("needs=") === 0) needs = seg[j].slice(6);
                if (seg[j].indexOf("ver=") === 0) ver = seg[j].slice(4);
            }
            out.push("<!-- ta-lib: path=" + rel + " sha256=" + h.sha256 + " needs=" + needs + " ver=" + ver + " st=draft -->");
            hit = true;
        } else { out.push(lines[i]); }
    }
    if (!hit) {
        if (out.length && out[out.length - 1] !== "") out.push("");
        out.push("<!-- ta-lib: path=" + rel + " sha256=" + h.sha256 + " needs=" + needs + " ver=" + ver + " st=draft -->");
    }
    tools.call("write_file", {path: "lib/INDEX.md", content: out.join("\n") + "\n"});
    return {path: rel, sha256: h.sha256, st: "draft"};
}
// The prelude's declarations live in the OUTER eval's scope — an indirectly
// eval'd lib source runs in GLOBAL scope and cannot see them (empirically:
// ReferenceError from the cycle test). Export the closed helper set onto
// globalThis so lib code gets the same environment the calling program has.
// User code may still shadow the names in its own scope; lib code binds to
// these globals (nesting, not scoping, per the design's prelude sketch).
globalThis.include = include;
globalThis.saveLib = saveLib;
globalThis.readAll = readAll;
globalThis.readLines = readLines;
globalThis.writeVerified = writeVerified;
"#;

/// The callBatch half of the prelude (code_llm_design §4/附录 A④, M2) —
/// appended by [`prelude_line`] only when `code.call_batch` is on (LD-15:
/// gate off = `typeof tools.callBatch === "undefined"`, the pre-batch
/// contract). The wrapper is THIN on purpose: the host runs every entry
/// through the exact single-call sequence; the only guest-side duty is the
/// envelope-array convention (§4) — `__ok:true` arms become parsed contents
/// (the same auto-parse-without-unwrap the single-call glue applies),
/// `__ok:false` arms become `Error` instances carrying `e.kind` / `e.tool` /
/// `e.retryable` / `e.halted` (the additive LD-9 field). `JSON.parse` can
/// never yield an Error instance, so a successful entry cannot fake a
/// failure and vice versa (the D12 collision argument).
const PRELUDE_BATCH_SRC: &str = r#"
function __ta_parse(s) {
    try { return JSON.parse(s); } catch (e) { return s; }
}
tools.callBatch = function (entries) {
    var arr = tools.call("__call_batch", { entries: entries });
    if (typeof arr === "string") arr = __ta_parse(arr);
    if (!arr || !Array.isArray(arr)) throw new Error("callBatch: unexpected host output");
    var out = [], i, env;
    for (i = 0; i < arr.length; i++) {
        env = arr[i];
        if (env && env.__ok === true) out.push(__ta_parse(env.content));
        else {
            var e = new Error(env && env.error ? env.error : "callBatch entry failed");
            if (env) {
                if (env.kind !== undefined) e.kind = env.kind;
                if (env.tool !== undefined) e.tool = env.tool;
                if (env.retryable !== undefined) e.retryable = env.retryable;
                if (env.halted !== undefined) e.halted = env.halted;
            }
            out.push(e);
        }
    }
    return out;
};
"#;

/// The prelude flattened to a single physical line (see PRELUDE_SRC docs).
/// Blank lines and `//`-comments are dropped, the rest joined with no
/// separator — every remaining line must therefore end with `;`, `{` or `}`
/// (the invariant test pins both properties; a separator-free join of
/// statements relying on ASI would silently glue two statements together).
/// `include_libs` / `call_batch` append the gated halves' flattened source
/// (MD-13 / LD-15: the gates decide existence, per-execution via snapshot).
fn prelude_line(include_libs: bool, call_batch: bool) -> String {
    fn flatten(src: &str) -> String {
        src.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("//"))
            .collect()
    }
    let mut out = flatten(PRELUDE_SRC);
    if include_libs {
        out.push_str(&flatten(PRELUDE_LIB_SRC));
    }
    if call_batch {
        out.push_str(&flatten(PRELUDE_BATCH_SRC));
    }
    out
}

fn guest_ctx(ctx: &ToolCtx) -> ToolCtx {
    ToolCtx {
        workdir: ctx.workdir.clone(),
        max_output_chars: GUEST_OUTPUT_CHARS,
        restrict_to_workdir: ctx.restrict_to_workdir,
        cancel: ctx.cancel.clone(),
        shell_timeout_secs: ctx.shell_timeout_secs,
        hooks: ctx.hooks.clone(),
        subagent: Some(CODE_MARKER.to_string()),
        budget: ctx.budget.clone(),
        deadline: ctx.deadline,
    }
}

/// §6 execution point ①: the hard wall clock = start + min(code.timeout_secs,
/// ctx.deadline remaining). `None` when BOTH sources are absent — no wall
/// clock, the documented boundary (cancel/fuel/mem still bound the run). An
/// already-past ctx deadline clamps to "now" (fires on the first epoch tick).
/// The deadline interrupt's user-facing reason — names the knob that bound
/// the run (cfg timeout, the parent run's remaining time, or both).
fn deadline_why(snap: &CodeConfig, bound_by_cfg: bool) -> String {
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
struct WallDeadline {
    at: Instant,
    from_cfg: bool,
}

fn wall_deadline(snap: &CodeConfig, ctx: &ToolCtx) -> Option<WallDeadline> {
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

// ── the tool ───────────────────────────────────────────────────────────────

/// The `run_code` tool. Construction snapshots the agent-facing config; the
/// shared `Arc<RwLock<CodeConfig>>` is the H2 carrier — the same Arc the
/// agent's `set_code_config` writes through, so hot updates reach the next
/// execution without touching the tool.
///
/// `Clone` exists for the single-construction-point rule (H3): `assemble`
/// builds ONE instance and hands CLONES of it to the tool table when
/// enabled — clones share the registry cache (`Arc<OnceLock>`) and the
/// config Arc, so the boxed instance the model dispatches to stays reachable
/// by the agent's switch methods.
#[derive(Clone)]
pub struct RunCodeTool {
    build_cfg: Config,
    /// Shared with the Agent's switch face (`set_code_config` etc.).
    /// pub(crate) so `Agent::sync_code_tool` can write through it — the H2
    /// invariant (only Agent and this tool see the Arc) is unchanged.
    pub(crate) code_cfg: Arc<RwLock<CodeConfig>>,
    client: reqwest::blocking::Client,
    api_key: String,
    mcp_tools: Vec<mcp::McpTool>,
    /// Host-injected extra tools (assemble's `extra_tools`, Arc-shared with
    /// the top-level list via [`crate::tools::SharedTool`]). The registry
    /// admits each only if the tool opts in via `available_to_code()` —
    /// default closed (v2 review).
    extra_tools: Vec<Arc<Box<dyn Tool>>>,
    /// Lazily-built inner registry, shared across clones. The failure (bad
    /// cfg.tools) is cached too — reported identically on every call.
    registry: Arc<OnceLock<Result<Registry, String>>>,
    /// Session registration set for the lib family (MD-14): shared across
    /// clones — every execution of this lineage admits against one set.
    lib_sessions: LibSessions,
}

impl RunCodeTool {
    pub fn new(
        cfg: &Config,
        code_cfg: Arc<RwLock<CodeConfig>>,
        client: &reqwest::blocking::Client,
        api_key: &str,
        mcp_tools: Vec<mcp::McpTool>,
        extra_tools: Vec<Arc<Box<dyn Tool>>>,
    ) -> Self {
        Self {
            build_cfg: cfg.clone(),
            code_cfg,
            client: client.clone(),
            api_key: api_key.to_string(),
            mcp_tools,
            extra_tools,
            registry: Arc::new(OnceLock::new()),
            lib_sessions: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// Read-lock → clone → release: the snapshot an execution runs with for
    /// its whole life (§8.6 — in-flight executions don't see mid-run updates).
    fn snapshot(&self) -> CodeConfig {
        self.code_cfg
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Built lazily on first use and cached (shared across clones — one
    /// build per lineage even after the tool table gains/loses instances).
    /// The failure (unknown tool in cfg.tools) is cached too — reported
    /// identically on every call.
    fn registry(&self) -> Result<&Registry> {
        let cell = &*self.registry;
        cell.get_or_init(|| {
            let build = || -> Result<Registry> {
                let mut tools = crate::tools::build(
                    &self.build_cfg.tools,
                    &self.build_cfg,
                    &self.client,
                    &self.api_key,
                    &self.mcp_tools,
                )?;
                // The shared MCP tools join the registry exactly as they join
                // the parent's tool list at assembly (§2; subagent.rs's
                // child_tools does the same extend): tools::build only hands
                // `shared_mcp` to the `task` arm — without this, every
                // `mcp_*` call from the sandbox is an unknown tool.
                tools.extend(
                    self.mcp_tools
                        .iter()
                        .cloned()
                        .map(|t| Box::new(t) as Box<dyn Tool>),
                );
                // Host-injected tools (bindings' CallbackTools, assemble's
                // extra_tools) join ONLY when the tool opts in via
                // `available_to_code()` — default closed (v2 review): a host
                // callback runs arbitrary host code, so sandbox visibility
                // must be an explicit per-tool authorization, not a side
                // effect of registering it top-level. The Arc-shared handle
                // IS the same instance the agent dispatches to.
                tools.extend(self.extra_tools.iter().map(|t| {
                    Box::new(SharedTool {
                        inner: Arc::clone(t),
                    }) as Box<dyn Tool>
                }));
                // The guest surface is its own capability axis (v2 review):
                // this used to piggyback on `available_to_subagent()`,
                // conflating two different exits. Curated explicitly now —
                // the file 四件套 + shell + MCP opt in; task/todo/run_code
                // and anything unannotated stay out.
                tools.retain(|t| t.available_to_code());
                // Guest-only primitives (never in the agent's top-level
                // list): copy_bytes — pushed AFTER the retain, which stays
                // about top-level/third-party tools.
                tools.push(Box::new(CopyBytesTool));
                let defs = tools.iter().map(|t| t.def()).collect();
                Ok(Registry {
                    tools: Arc::new(tools),
                    defs: Arc::new(defs),
                })
            };
            build().map_err(|e| format!("{e:#}"))
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Evaluate guest code under the snapshot's limits. Returns the tool
    /// output; `is_error` covers both JS exceptions and sandbox terminations.
    fn eval(&self, code: &str, ctx: &ToolCtx, emit: &mut dyn FnMut(AgentEvent)) -> ToolOutput {
        let snap = self.snapshot();
        let mem_cap = (snap.mem_mb as usize)
            .saturating_mul(1024 * 1024)
            .max(MODULE_INITIAL_MEMORY); // below the linked initial we cannot go
        let registry = match self.registry() {
            Ok(r) => r,
            Err(e) => {
                return ToolOutput::error(format!("run_code: tool registry build failed: {e:#}"))
            }
        };
        // Warm the process-level module OnceLock BEFORE the wall clock arms
        // (sec-review m-2): a cold process's first execution pays wasmtime's
        // lazy compilation here (~10s debug) — armed first, that window sits
        // between deadline computation and the first epoch check point and
        // the run overshoots its wall clock by the compile time, invisibly.
        if let Err(e) = module() {
            return ToolOutput::error(format!("run_code: guest init failed: {e:#}"));
        }
        // §6 execution point ①: the hard wall clock, and the epoch machinery
        // that delivers cancel + deadline into a tight wasm loop.
        ensure_ticker();
        let wall = wall_deadline(&snap, ctx);

        // Execution point ⓪ — the entry gate. The epoch callback below can
        // only observe an interrupt at the NEXT ticker tick (~10ms), and a
        // trivial guest program legitimately finishes inside that window —
        // so a deadline already past, or a cancel flag already set, must be
        // caught HERE, before any guest execution is admitted (the same
        // gate-before-start discipline as the batch worker / subagent spawn).
        if let Some(src) = wall
            .filter(|w| Instant::now() >= w.at)
            .map(|_| Interrupt::Deadline)
            .or_else(|| {
                ctx.cancel
                    .as_ref()
                    .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
                    .then_some(Interrupt::Cancel)
            })
        {
            let why = match src {
                Interrupt::Cancel => {
                    "cancelled (host cancel flag) — stopped before the sandbox started".to_string()
                }
                // An already-past deadline at entry is by definition the
                // binding side — attribute honestly either way.
                Interrupt::Deadline => {
                    deadline_why(&snap, wall.map(|w| w.from_cfg).unwrap_or(false))
                }
            };
            return ToolOutput {
                is_error: true,
                usage: None,
                usage_in_shared_budget: false,
                content: format!("[run_code] terminated: {why}"),
                image: None,
                // Same audit shape as the in-flight path: zero calls, zero
                // fuel — the guest never executed.
                meta: Some(audit_meta(&[], &snap, 0, false, 0)),
            };
        }

        // SAFETY: erases the lifetime on the fat pointer to satisfy Store<T>'s
        // `T: 'static` bound. Sound because the Store never outlives this
        // frame — it is dropped before `eval` returns, while `emit`'s borrow
        // is still live — and wasmtime runs imports synchronously on this
        // thread inside the `eval_fn.call` below, strictly within that
        // window. No copy of the pointer escapes the Store. This is the same
        // reentrancy-through-FFI shape the language bindings use for their
        // host callbacks.
        let emit_ptr: *mut (dyn FnMut(AgentEvent) + 'static) = unsafe {
            std::mem::transmute::<*mut dyn FnMut(AgentEvent), *mut (dyn FnMut(AgentEvent) + 'static)>(
                emit,
            )
        };
        let host = HostCtx::new(
            emit_ptr,
            registry,
            guest_ctx(ctx),
            mem_cap,
            snap.clone(),
            wall,
            Arc::clone(&self.lib_sessions),
        );

        let engine = match engine() {
            Ok(e) => e,
            Err(e) => return ToolOutput::error(format!("run_code: {e:#}")),
        };
        let mut store = Store::new(engine, host);
        if let Err(e) = store.set_fuel(snap.fuel) {
            return ToolOutput::error(format!("run_code: fuel not available: {e:#}"));
        }
        store.limiter(|d| &mut d.limiter);
        // Arm the epoch deadline: fires after the NEXT epoch tick, then the
        // callback below either interrupts or renews. The ~10ms tick is the
        // delivery granularity — cancel/wall-clock land within ~10ms of
        // firing even in a tight wasm loop.
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|mut store| {
            // Decide under one borrow of the store data…
            let fire = {
                let d = store.data_mut();
                if d.interrupt.is_some() {
                    d.interrupt
                } else if d.deadline.is_some_and(|w| Instant::now() >= w.at) {
                    d.interrupt = Some(Interrupt::Deadline);
                    d.interrupt
                } else if d
                    .ctx
                    .cancel
                    .as_ref()
                    .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
                {
                    d.interrupt = Some(Interrupt::Cancel);
                    d.interrupt
                } else {
                    None
                }
            };
            match fire {
                Some(_) => {
                    // The interrupt delivery vehicle. wasmtime 36's
                    // `UpdateDeadline` has no trap variant, so the deadline
                    // callback cannot refuse to renew — but it CAN return
                    // `Err`, which propagates out of the `new_epoch` libcall
                    // as the error of the in-flight wasm call. (Zeroing the
                    // fuel does NOT work: the running function caches fuel in
                    // a wasm local and flushes its cache back over the store
                    // slot at its next check point, clobbering the zero —
                    // verified empirically, the guest ran 25s past the
                    // interrupt.) `terminated` attributes via the
                    // `interrupt` flag, not this message.
                    Err(wasmtime::Error::msg(
                        "run_code: interrupted by host (cancel or wall clock)",
                    ))
                }
                // 1 epoch tick ≈ 10ms (the ticker's period) — re-check at that
                // cadence for as long as the guest keeps executing. Delivery
                // lands at the next epoch check point: ~10ms for ordinary
                // code; an empty bytecode busy-loop's sparsest check points
                // are quickjs's internal interrupt-poll sites (larger, still
                // bounded — and the fuel budget backstops it).
                None => Ok(wasmtime::UpdateDeadline::Continue(1)),
            }
        });

        let instance = match linker().and_then(|l| l.instantiate(&mut store, module()?)) {
            Ok(i) => i,
            Err(e) => return ToolOutput::error(format!("run_code: guest init failed: {e:#}")),
        };

        // Reactor ctors — must run before any other export (§5.1). quickjs's
        // stdlib init is heavy enough (debug builds: >1s) that a short wall
        // clock can legitimately fire HERE — so like every other guest call,
        // its errors go through `terminated` for source attribution.
        let init = instance.get_typed_func::<(), ()>(&mut store, "_initialize");
        if let Err(trap) = init.and_then(|f| f.call(&mut store, ())) {
            return self.terminated(&mut store, &snap, trap);
        }

        let alloc = match instance.get_typed_func::<i32, i32>(&mut store, "alloc") {
            Ok(f) => f,
            Err(e) => return ToolOutput::error(format!("run_code: {e:#}")),
        };
        let eval_fn = match instance.get_typed_func::<(i32, i32), i32>(&mut store, "eval_code") {
            Ok(f) => f,
            Err(e) => return ToolOutput::error(format!("run_code: {e:#}")),
        };
        let mem = match instance.get_memory(&mut store, "memory") {
            Some(m) => m,
            None => return ToolOutput::error("run_code: guest exports no memory"),
        };

        // ── Guest prelude ───────────────────────────────────────────────────
        // Prepended on ONE physical line (see prelude_line docs) so user stack
        // frames shift by a constant +1 instead of the prelude's full length.
        // Same function-body scope as the user code: the helper declarations
        // hoist ahead of it, and user code may shadow them freely.
        let full_source = format!("{}\n{}", prelude_line(snap.include, snap.call_batch), code);
        let code_bytes = full_source.as_bytes();
        // alloc → write code → eval, sharing one error path: any trap here is
        // a termination (fuel/mem), not a JS outcome.
        let rc = match alloc
            .call(&mut store, code_bytes.len() as i32)
            .and_then(|p| {
                if p == 0 {
                    return Err(wasmtime::Error::msg(
                        "guest alloc returned 0 (heap exhausted)",
                    ));
                }
                mem.write(&mut store, p as usize, code_bytes)?;
                eval_fn.call(&mut store, (p, code_bytes.len() as i32))
            }) {
            Ok(rc) => rc,
            Err(trap) => return self.terminated(&mut store, &snap, trap),
        };

        let outcome = if rc == 0 {
            let packed = match instance
                .get_typed_func::<(), i64>(&mut store, "take_result")
                .and_then(|f| f.call(&mut store, ()))
            {
                Ok(p) => p,
                Err(trap) => return self.terminated(&mut store, &snap, trap),
            };
            let (p, l) = unpack(packed);
            read_memory_string(&mem, &mut store, p, l).unwrap_or_default()
        } else {
            let packed = match instance
                .get_typed_func::<(), i64>(&mut store, "take_last_error")
                .and_then(|f| f.call(&mut store, ()))
            {
                Ok(p) => p,
                Err(trap) => return self.terminated(&mut store, &snap, trap),
            };
            let (p, l) = unpack(packed);
            read_memory_string(&mem, &mut store, p, l).unwrap_or_default()
        };

        let logs = std::mem::take(&mut store.data_mut().logs);
        let usage = std::mem::take(&mut store.data_mut().usage);
        let mut value = outcome;
        if rc != 0 {
            // The linear-memory limiter denies growth; malloc then fails and
            // quickjs surfaces that as a bare JS "InternalError: out of
            // memory" with no numbers — attribute it via the flag (§6).
            // The flag is a sticky latch (first denial sets it for the run),
            // so also require the error text to actually look like the OOM
            // InternalError: a guest that CAUGHT the OOM and later failed for
            // an unrelated reason must keep its real error, not be
            // re-attributed to mem (correctness-review n-5). On a wording
            // drift in quickjs this falls to the JS-error branch, which shows
            // the guest's own text — the honest direction to fail open to.
            // Report the EFFECTIVE floor: mem_cap can never go below the
            // module's linked initial memory (sec-review n-3).
            value = if store.data().limiter.exceeded && value.contains("out of memory") {
                format!(
                    "[run_code] terminated: memory limit exceeded ({} MB — raise code.mem_mb)",
                    snap.mem_mb
                        .max((MODULE_INITIAL_MEMORY / (1024 * 1024)) as u32)
                )
            } else {
                format!("[run_code] JS error: {value}")
            };
        }
        let (content, truncated) = assemble_with_logs(value, &logs, snap.max_result_chars);
        // get_fuel errors only when fuel isn't enabled (it always is); both
        // finish paths report the FULL budget burned in that unreachable
        // case — conservative for audit honesty (correctness-review #9).
        let fuel_used = snap.fuel.saturating_sub(store.get_fuel().unwrap_or(0));
        let logs_dropped = store.data().logs_dropped;
        let meta = audit_meta(
            &store.data().calls,
            &snap,
            fuel_used,
            truncated,
            logs_dropped,
        );
        ToolOutput {
            is_error: rc != 0,
            // Aggregated inner-call spend: every token of it was shared into
            // the budget counter at the inner fold point (see host_call's
            // FR-10 fold), so the parent's fold takes totals only.
            usage: (usage.total() > 0).then_some(usage),
            usage_in_shared_budget: true,
            content,
            image: None,
            meta: Some(meta),
        }
    }

    /// A trap terminated the guest (cancel / wall clock / fuel / memory /
    /// wasm stack). Attribute it via the independent source flags (§6) — the
    /// trap itself carries none; the epoch-delivered interrupts (cancel /
    /// deadline) outrank the native signals in the check order.
    fn terminated(
        &self,
        store: &mut Store<HostCtx>,
        snap: &CodeConfig,
        trap: anyhow::Error,
    ) -> ToolOutput {
        let logs = std::mem::take(&mut store.data_mut().logs);
        let usage = std::mem::take(&mut store.data_mut().usage);
        let why = if let Some(src) = store.data().interrupt {
            match src {
                Interrupt::Cancel => {
                    // Honest boundary (FR-9): an inner tool call already in
                    // flight when the flag fired runs to completion — the
                    // epoch trap lands at the next wasm instruction.
                    "cancelled (host cancel flag) — stopped at the next \
                     instruction boundary; any in-flight inner tool call \
                     ran to completion"
                        .to_string()
                }
                Interrupt::Deadline => deadline_why(
                    snap,
                    store.data().deadline.map(|w| w.from_cfg).unwrap_or(false),
                ),
            }
        } else if store.data().limiter.exceeded {
            format!(
                "memory limit exceeded ({} MB — raise code.mem_mb)",
                snap.mem_mb
                    .max((MODULE_INITIAL_MEMORY / (1024 * 1024)) as u32)
            )
        } else if store.get_fuel().is_ok_and(|f| f == 0) {
            format!(
                "fuel exhausted ({} — deterministic budget; simplify the code \
                 or raise code.fuel)",
                snap.fuel
            )
        } else if trap
            .root_cause()
            .downcast_ref::<wasmtime::Trap>()
            .is_some_and(|t| matches!(t, wasmtime::Trap::StackOverflow))
        {
            // quickjs-ng hard-disables its own stack limit under __wasi__, so
            // deep recursion bottoms out here instead of as a JS RangeError.
            "call stack exhausted (deep recursion)".to_string()
        } else {
            format!("{trap:#}")
        };
        let (content, fitted_truncated) = assemble_with_logs(
            format!("[run_code] terminated: {why}"),
            &logs,
            snap.max_result_chars,
        );
        // A terminated run still audited its completed calls — attach the
        // same trace so result.json shows how far it got before the limit.
        // Fuel is approximate by up to one check-point chunk: the
        // interrupted function caches fuel in a wasm local and never runs
        // its exit-flush, so the store slot holds the last flushed value.
        let fuel_used = snap.fuel.saturating_sub(store.get_fuel().unwrap_or(0));
        let logs_dropped = store.data().logs_dropped;
        let meta = audit_meta(
            &store.data().calls,
            snap,
            fuel_used,
            fitted_truncated,
            logs_dropped,
        );
        ToolOutput {
            is_error: true,
            // Same as the success path: the aggregate was counter-shared at
            // the inner fold points before the trap.
            usage: (usage.total() > 0).then_some(usage),
            usage_in_shared_budget: true,
            content,
            image: None,
            meta: Some(meta),
        }
    }
}

/// Read a `(ptr, len)` range out of linear memory as an owned String (lenient
/// lossy decode — the guest serializes UTF-8, but a truncated buffer from a
/// killed run must not panic the host).
fn read_memory_string(
    mem: &Memory,
    store: &mut Store<HostCtx>,
    ptr: usize,
    len: usize,
) -> Option<String> {
    if len > (1 << 24) {
        return None;
    }
    let mut buf = vec![0u8; len];
    mem.read(&*store, ptr, &mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// The gate-ON description overlay (MD-13): appended to the base description
/// when `code.include` is set. A separate const so the extractor in
/// scripts/sync_run_code_contract.py — which pins the gate-OFF r# literal —
/// keeps matching exactly one description. LLM-visible copy is spec: this
/// text is mirrored in docs/code_memory_design.md's appendix; the two change
/// together through its revision log.
const LIB_DESC_SUFFIX: &str = "\n\n【库】（仅 code.include 开启时；关闭时上句成立、无以下助手）include(path)：加载 lib/ 下已登记的代码库到全局作用域（幂等，循环依赖自动截断，失败 throw）；saveLib(path, src)：写库文件并在 lib/INDEX.md 登记（恒 st=draft，本会话即可 include；跨会话复用需人工审校后将该行 st 改为 ok）。库校验失败（未登记/内容与登记不符/超限）e.kind=\"lib\"，重试无意义。建库/用库/修库生命周期详见 run-code 技能的「库与复用」段。";

/// The call_batch gate-ON description overlay (LD-15/附录 A④): appended when
/// `code.call_batch` is set, outside the pinned r# literal (same extractor
/// discipline as [`LIB_DESC_SUFFIX`]). Copy is A④ finalized at landing — the
/// draft's "read_file / llm" lists `llm`, which does not exist in the
/// registry until M3; naming it now would break the closed-list
/// anti-hallucination contract, so v1 says "read_file 等" (M3 revisits,
/// through this file's doc + the design revision log).
const BATCH_DESC_SUFFIX: &str = "\n\n【批量】（仅 code.call_batch 开启时；关闭时无此助手）tools.callBatch([{name, args}, …])：并发执行一批只读工具调用（read_file 等 batchable 工具；按提交序返回数组；无原子性、无回滚、部分执行是正常结果——逐条检查）。单项失败不抛、该条目为 Error 实例带 e.kind/e.tool——用 instanceof 判别，不要 JSON.stringify 失败条目（Error 序列化成 {}，细节丢失）。并发上限 8/波；分页拉取与逐项判别是理想对象；副作用调用不在批内——写顺序 for 循环。详见 run-code 技能的「批量调用」段。";

impl Tool for RunCodeTool {
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }

    fn def(&self) -> ToolDef {
        // Slim description (v2 P2): the description rides in EVERY request's
        // tool schema, so it carries identity + environment + the CLOSED
        // helper list + a pointer to the built-in run-code skill — the deep
        // contracts (read_file's three modes, byte-exact copy, examples) live
        // in run_code_skill.md, pulled on demand via read_skill. The spec is
        // "slim description + run_code_skill.md" together: the helper list
        // must stay closed and named here (anti-hallucination surface), and
        // prelude/description/skill.md change together.
        //
        // The r#-literal below is the gate-OFF contract — byte-identical to
        // the pre-include/pre-batch binary; scripts/sync_run_code_contract.py
        // pins THIS text, so both overlays append OUTSIDE the literal
        // (code.include on → 【库】 section, MD-13; code.call_batch on →
        // 【批量】 section, LD-15; the closed-list sentence stays true because
        // each overlay explicitly scopes itself).
        let base = ToolDef {
        name: "run_code".into(),
            description: r#"在进程内 JS 沙箱（QuickJS，同步 ES2023）编排 MCP/API 分页、批量操作和大文件处理。规则已明确，剩余仍有跨页/大量对象的同构调用或大量中间数据处理时，先 read_skill({"skill":"run-code"})，再用代码承接。可先直接探查接口，拿到结果后重判；只续做未完成部分。整个剩余任务一次工具调用或一条可用 shell 管道即可完成时直接做；小集合或逐项需语义判断时不必用代码。不能仅凭“所有/批量”触发。

【环境】无 async/await、Promise、timer、import/fetch、网络与文件系统直访（读写经工具）；Date.now/Math.random 不可用。调用次数/内存/时长有限，超限终止；调用额度每次重计，超了分批续跑。

【API】
- 内置助手（前导函数，直接可用；此外没有别的内置助手）：readAll(path) 整读文本（约 200 万字符内一次读完）、readLines(path) 行数组、writeVerified(path, content) 字节保真写（写后读回校验）
- tools.call(工具名, 参数对象)：调本会话工具（含 MCP，不含 task/todo/run_code），失败 throw 可 catch。JSON 自动解析但不解包：{response:[...]} 取 .response；循环前确认形状。失败调用也扣额度
- tools.list()：调 MCP 前先核对准确名字（`_` 与 `__` 易混），返回 [{name, description}]；console.log(...)：调试输出，实时显示并随结果返回
- tools.budget()：查剩余额度（次数/墙钟/燃料/token）——长批量前查、循环里定期查
- copy_bytes(src,dst)（经 tools.call）：host 流式字节保真复制文件（不经 JS 字符串）
- 工具错误可 catch 且带 e.kind（denied/hooks/budget/deadline/unknown_tool/tool）；retryable=false 类重试无意义

【返回】return 的值经 JSON 序列化作为工具结果。必须返回紧凑摘要（统计数字、文件路径、少量样例行）；大数据写文件后返回路径，禁止把数据本体放进返回值。

【数值】Number 是 float64，整数乘法/累加超过 2^53 会静默失真。移植 Python 等整数算术（计数器、LCG、哈希、大数）用 BigInt 字面量（123456789n、2n**31n）。

read_file 三模式（text / format:"json" / raw）、字节保真复制及分页校验示例见 run-code；写非平凡代码前先读。"#
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "完整 JS 源码；return 的值作为结果"
                    }
                },
                "required": ["code"]
            }),
        };
        let snap = self.snapshot();
        let description = match (snap.include, snap.call_batch) {
            (false, false) => return base,
            (true, false) => format!("{}{LIB_DESC_SUFFIX}", base.description),
            (false, true) => format!("{}{BATCH_DESC_SUFFIX}", base.description),
            (true, true) => format!("{}{LIB_DESC_SUFFIX}{BATCH_DESC_SUFFIX}", base.description),
        };
        ToolDef {
            description,
            ..base
        }
    }

    fn run_with_emit(
        &self,
        input: &Value,
        ctx: &ToolCtx,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<ToolOutput> {
        let Some(code) = input.get("code").and_then(Value::as_str) else {
            return Ok(ToolOutput::error("run_code: missing string field 'code'"));
        };
        if code.len() > MAX_CODE_BYTES {
            return Ok(ToolOutput::error(format!(
                "run_code: code exceeds {MAX_CODE_BYTES} bytes"
            )));
        }
        // FR-2 预检（finetune_support 设计 §4.1）：调用期决策——每次调用经
        // snapshot() 读锁生效，与资源限制字段同先例（FD-7 不对称的调用期一
        // 端）。命中即 ToolOutput::error 拒答，先于 eval：registry/wasm 零接
        // 触、落 Slot::Ran(error)、进 records、不占预算（hooks-Deny 先例）。
        if self.snapshot().precheck {
            if let Some(msg) = precheck(code) {
                return Ok(ToolOutput::error(msg));
            }
        }
        Ok(self.eval(code, ctx, emit))
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        self.run_with_emit(input, ctx, &mut |_| {})
    }

    fn concurrency(&self) -> ToolConcurrency {
        // Each call is a fresh Store; concurrent calls share only the
        // read-only Engine/Module/Linker and the lazily-built registry
        // (OnceLock). Parallel-safe, but Sequential by default matches every
        // other stateful tool — the loop opts in per tool only when the
        // implementation asks for it, and a fan-out of sandboxes is exactly
        // the memory multiplier `mem_mb` accounting assumes it isn't doing.
        ToolConcurrency::Sequential
    }

    fn available_to_subagent(&self) -> bool {
        false // nested sandboxes buy nothing (§2 registry): task children don't see run_code
    }
}

// ── FR-2 precheck (finetune_support 设计 §4) ────────────────────────────────
//
// 静态预检是效率/体验特性，不是安全机制（§4.3 诚实声明）：词法扫描可被混淆
// 绕过（globalThis["Date"]["now"]() 放行），漏检的代价仅是退回运行时失败，
// 不少任何安全保证。检出集合（FD-9 修正后）与拒答文案（附录 A，即规格）的
// 变更必须先走设计稿修订记录、再改码。

/// Lexer token — just enough for the three detection rules (§4.2). NOT a JS
/// parser: strings/templates/comments are skipped, regex literals get the
/// standard expression-position heuristic so `/Date\.now/` in a regex body
/// cannot produce a false member-expression hit.
#[derive(Debug)]
enum Tok {
    /// Identifier or keyword text and its 1-based start line.
    Ident(String, usize),
    /// String literal content with quotes stripped but escapes NOT decoded,
    /// and its start line. The reserved-name comparison only matches plain
    /// ASCII names; an escaped spelling decodes to a reserved name only at
    /// runtime, where the reserved-name intercept still rejects it (safe
    /// direction).
    Str(String, usize),
    /// One ASCII punctuation char (no line: detection patterns never report a
    /// punct's own line — the leading ident or string literal carries it).
    Punct(char),
}

/// Keywords after which a `/` opens a regex literal, not division (an
/// expression position follows them). Not the full ECMA list — only the ones
/// that can directly precede an expression.
const EXPR_KEYWORDS: &[&str] = &[
    "return",
    "typeof",
    "case",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "else",
    "do",
    "yield",
    "await",
    "instanceof",
];

struct Lexer<'a> {
    b: &'a [u8],
    i: usize,
    line: usize,
    toks: Vec<Tok>,
    /// What the previous significant token implies for `/`: value-like
    /// (ident, string, number, `)`/`]`/`}`, template, regex) → division;
    /// anything else → regex start.
    prev_value: bool,
}

impl Lexer<'_> {
    /// Scan until EOF, or — when `in_expr` — until the `}` that closes a
    /// template `${…}` (consumed; the caller resumes template-literal mode).
    fn run(&mut self, in_expr: bool) {
        let mut depth = 0usize;
        while self.i < self.b.len() {
            let c = self.b[self.i];
            match c {
                b'\n' => {
                    self.line += 1;
                    self.i += 1;
                }
                b' ' | b'\t' | b'\r' => self.i += 1,
                b'/' if self.b.get(self.i + 1) == Some(&b'/') => {
                    while self.i < self.b.len() && self.b[self.i] != b'\n' {
                        self.i += 1;
                    }
                }
                b'/' if self.b.get(self.i + 1) == Some(&b'*') => {
                    self.i += 2;
                    while self.i + 1 < self.b.len()
                        && !(self.b[self.i] == b'*' && self.b[self.i + 1] == b'/')
                    {
                        if self.b[self.i] == b'\n' {
                            self.line += 1;
                        }
                        self.i += 1;
                    }
                    self.i = (self.i + 2).min(self.b.len());
                }
                b'/' if !self.prev_value => self.regex(),
                b'\'' | b'"' => self.string(c),
                b'`' => {
                    self.i += 1;
                    self.template();
                    self.prev_value = true;
                }
                b'{' => {
                    depth += 1;
                    self.punct('{');
                }
                b'}' if in_expr && depth == 0 => {
                    self.i += 1;
                    return;
                }
                b'}' => {
                    depth = depth.saturating_sub(1);
                    self.punct('}');
                }
                _ if c == b'_' || c == b'$' || c.is_ascii_alphabetic() => self.ident(),
                _ if c.is_ascii_digit() => {
                    while self.i < self.b.len()
                        && (self.b[self.i].is_ascii_alphanumeric()
                            || matches!(self.b[self.i], b'.' | b'_'))
                    {
                        self.i += 1;
                    }
                    self.prev_value = true;
                }
                _ if c.is_ascii() => {
                    let ch = c as char;
                    self.punct(ch);
                }
                _ => self.i += 1, // UTF-8 continuation byte outside a string: skip
            }
        }
    }

    fn punct(&mut self, ch: char) {
        self.toks.push(Tok::Punct(ch));
        self.prev_value = matches!(ch, ')' | ']' | '}');
        self.i += 1;
    }

    fn ident(&mut self) {
        let start = self.i;
        while self.i < self.b.len() {
            let d = self.b[self.i];
            if d == b'_' || d == b'$' || d.is_ascii_alphanumeric() {
                self.i += 1;
            } else {
                break;
            }
        }
        let text = String::from_utf8_lossy(&self.b[start..self.i]).into_owned();
        self.prev_value = !EXPR_KEYWORDS.contains(&text.as_str());
        self.toks.push(Tok::Ident(text, self.line));
    }

    fn string(&mut self, quote: u8) {
        let start_line = self.line;
        self.i += 1;
        let content_start = self.i;
        while self.i < self.b.len() && self.b[self.i] != quote {
            match self.b[self.i] {
                b'\\' if self.i + 1 < self.b.len() => self.i += 2,
                b'\n' => {
                    // A raw newline in a '…'/"…" string is invalid JS; count
                    // the line and keep scanning (recovery, not parsing).
                    self.line += 1;
                    self.i += 1;
                }
                _ => self.i += 1,
            }
        }
        let content = String::from_utf8_lossy(&self.b[content_start..self.i]).into_owned();
        if self.i < self.b.len() {
            self.i += 1; // closing quote
        }
        self.toks.push(Tok::Str(content, start_line));
        self.prev_value = true;
    }

    /// Regex literal body + flags (entry: `self.i` is ON the opening `/`).
    /// Character classes are tracked so `/[/]/` terminates correctly; a raw
    /// newline ends the scan (recovery — regexes can't span lines).
    fn regex(&mut self) {
        self.i += 1;
        let mut in_class = false;
        while self.i < self.b.len() {
            match self.b[self.i] {
                b'\\' => self.i = (self.i + 2).min(self.b.len()),
                b'[' => {
                    in_class = true;
                    self.i += 1;
                }
                b']' => {
                    in_class = false;
                    self.i += 1;
                }
                b'/' if !in_class => {
                    self.i += 1;
                    break;
                }
                b'\n' => break,
                _ => self.i += 1,
            }
        }
        while self.i < self.b.len() && self.b[self.i].is_ascii_alphabetic() {
            self.i += 1; // flags
        }
        self.prev_value = true;
    }

    /// Template literal (entry: `self.i` is just past the opening backtick).
    /// Literal parts are skipped; `${…}` expressions are lexed normally via
    /// recursive `run(true)`, so hits inside them get their real line numbers.
    fn template(&mut self) {
        while self.i < self.b.len() {
            match self.b[self.i] {
                b'\\' => self.i = (self.i + 2).min(self.b.len()),
                b'`' => {
                    self.i += 1;
                    return;
                }
                b'$' if self.b.get(self.i + 1) == Some(&b'{') => {
                    self.i += 2;
                    self.prev_value = false; // expression position starts fresh
                    self.run(true);
                }
                b'\n' => {
                    self.line += 1;
                    self.i += 1;
                }
                _ => self.i += 1,
            }
        }
    }
}

fn lex(code: &str) -> Vec<Tok> {
    let mut lx = Lexer {
        b: code.as_bytes(),
        i: 0,
        line: 1,
        toks: Vec::new(),
        prev_value: false,
    };
    lx.run(false);
    lx.toks
}

/// Per-class hit cap (FD-11): beyond it the copy says `等 N 处` with N = the
/// class's TOTAL hit count, so one rejection still reports the true scale.
const PRECHECK_HIT_CAP: usize = 20;

/// The FR-2 static gate (§4.2): a single lexer pass plus a fixed-order render
/// of every hit (FD-11). Returns the appendix-A rejection copy, or `None` when
/// the code is clean. Pure — unit tests call it directly and it touches
/// nothing outside its input (NFR-4).
///
/// Detection sets (FD-9 修正后，需求方数据侧同款约束）:
/// - env syntax: keywords `async`/`await`/`import` (no exemption); shadowable
///   globals `Promise`/`setTimeout`/`fetch` only when used as a call /
///   construct / member target (`X(`、`new X`、`X.`) AND not declared in-file.
/// - determinism: member expressions `Date.now` / `Math.random` (no exemption
///   — shadowing those names is pathological, and a miss only degrades to the
///   runtime stub, the safe direction).
/// - reserved tools: `tools.call("…")` with a string-literal first arg in
///   {`run_code`, `task`, `todo`}. `__tools_list`/`__budget` are deliberately
///   NOT in the set: they are WORKING read-only introspection intercepts (the
///   prelude's own `tools.budget()` routes through `__budget`) — rejecting
///   them would make the copy lie AND false-positive on legit code. A
///   non-literal first arg is not detected either; it falls through to the
///   runtime reserved-name intercept (safe direction again).
fn precheck(code: &str) -> Option<String> {
    let toks = lex(code);
    // Declaration → exemption (FD-10): the name right after a decl keyword is
    // the model's own. The set covers the WHOLE file (hoisting order ignored),
    // then the hit pass filters by it. Known imprecision, accepted by design:
    // multi-declarator (`let a, fetch`) and destructuring don't register.
    let mut declared = std::collections::HashSet::new();
    for w in toks.windows(2) {
        if let [Tok::Ident(kw, _), Tok::Ident(name, _)] = w {
            if matches!(kw.as_str(), "function" | "const" | "let" | "var" | "class") {
                declared.insert(name.as_str());
            }
        }
    }

    let mut env: Vec<(String, usize)> = Vec::new();
    let mut det: Vec<(String, usize)> = Vec::new();
    let mut reserved: Vec<(String, usize)> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        let Tok::Ident(name, line) = t else { continue };
        // 环境语法：keywords fire unconditionally.
        if matches!(name.as_str(), "async" | "await" | "import") {
            env.push((name.clone(), *line));
            continue;
        }
        // 确定性：Date.now / Math.random member expressions.
        if matches!(name.as_str(), "Date" | "Math") {
            if let (Some(Tok::Punct('.')), Some(Tok::Ident(member, _))) =
                (toks.get(i + 1), toks.get(i + 2))
            {
                if (name == "Date" && member == "now") || (name == "Math" && member == "random") {
                    det.push((format!("{name}.{member}"), *line));
                    continue;
                }
            }
        }
        // 保留工具：tools.call("<reserved>") — the reported line is the string
        // literal's own (points at the reserved name itself).
        if name == "tools" {
            if let (
                Some(Tok::Punct('.')),
                Some(Tok::Ident(method, _)),
                Some(Tok::Punct('(')),
                Some(Tok::Str(arg, arg_line)),
            ) = (
                toks.get(i + 1),
                toks.get(i + 2),
                toks.get(i + 3),
                toks.get(i + 4),
            ) {
                if method == "call" && matches!(arg.as_str(), "run_code" | "task" | "todo") {
                    reserved.push((arg.clone(), *arg_line));
                    continue;
                }
            }
        }
        // 可遮蔽全局名：call/construct/member target AND not declared in-file.
        if matches!(name.as_str(), "Promise" | "setTimeout" | "fetch")
            && !declared.contains(name.as_str())
        {
            let as_target = matches!(toks.get(i + 1), Some(Tok::Punct('(' | '.')))
                || i.checked_sub(1)
                    .and_then(|p| toks.get(p))
                    .is_some_and(|p| matches!(p, Tok::Ident(kw, _) if kw == "new"));
            if as_target {
                env.push((name.clone(), *line));
            }
        }
    }
    render_precheck(&env, &det, &reserved)
}

fn fmt_precheck_hits(hits: &[(String, usize)]) -> String {
    let mut s = hits
        .iter()
        .take(PRECHECK_HIT_CAP)
        .map(|(t, l)| format!("\"{t}\"（第 {l} 行）"))
        .collect::<Vec<_>>()
        .join("、");
    if hits.len() > PRECHECK_HIT_CAP {
        s.push_str(&format!("、等 {} 处", hits.len()));
    }
    s
}

/// 附录 A 即规格：以下文案逐字照抄设计稿附录 A-1/A-2/A-3，变更须先走设计稿
/// 修订记录。段序固定：环境语法 → 确定性 → 保留工具（FD-11）。
fn render_precheck(
    env: &[(String, usize)],
    det: &[(String, usize)],
    reserved: &[(String, usize)],
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if !env.is_empty() {
        parts.push(format!(
            "run_code precheck: 检出环境不支持的语法 {}——本沙箱是同步环境：没有 async/await/Promise，没有 setTimeout 等 timer，没有 import/fetch。请改同步写法：顺序执行；批量数据在一次执行内用循环 + tools.call 逐次处理。代码未执行，无副作用；修正后重发完整代码。",
            fmt_precheck_hits(env)
        ));
    }
    if !det.is_empty() {
        parts.push(format!(
            "run_code precheck: 检出确定性违规 {}——确定性沙箱不提供 Date.now / Math.random（调用即在运行时抛错，不是返回固定值的桩）。需要时间或随机值时，改用数据记录自带的字段或代码内常量。代码未执行，无副作用；修正后重发完整代码。",
            fmt_precheck_hits(det)
        ));
    }
    // A-3 逐处一段（文案含 {name}/{line} 模板位，无法合并列举）。
    for (name, line) in reserved.iter().take(PRECHECK_HIT_CAP) {
        parts.push(format!(
            "run_code precheck: 检出 tools.call(\"{name}\")（第 {line} 行）——\"{name}\" 在沙箱内不可达（设计 D11：防自递归与嵌套成本）。沙箱内只能调用 tools.list() 列出的工具；\"{name}\" 的调用请移出沙箱、由主循环直接发起。代码未执行，无副作用；修正后重发完整代码。"
        ));
    }
    if reserved.len() > PRECHECK_HIT_CAP {
        parts.push(format!(
            "run_code precheck: 保留工具命中共 {} 处，以上仅列前 {PRECHECK_HIT_CAP} 处。代码未执行，无副作用；修正后重发完整代码。",
            reserved.len()
        ));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A scratch workdir per TEST (guest file tools resolve against the ctx
    /// workdir; the repo cwd must not be touched). Unique per call: tests in
    /// one binary share a pid, and a shared dir let one test's `out/` entries
    /// collide with another's symlink setup.
    fn scratch() -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "ta-run-code-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn tool_with(code_cfg: CodeConfig) -> RunCodeTool {
        tool_with_tools(code_cfg, r#"["read_file","list_dir"]"#)
    }

    /// [`tool_with`] with an explicit whitelist (tests that need write_file
    /// or shell in the guest registry).
    fn tool_with_tools(code_cfg: CodeConfig, tools: &str) -> RunCodeTool {
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":{tools}}}"#
        ))
        .unwrap();
        RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(code_cfg)),
            &reqwest::blocking::Client::new(),
            "k",
            vec![],
            vec![],
        )
    }

    fn ctx(workdir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            workdir: workdir.to_path_buf(),
            max_output_chars: 1000,
            restrict_to_workdir: false,
            cancel: None,
            shell_timeout_secs: 5,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
        }
    }

    fn run_code(code: &str, code_cfg: CodeConfig) -> ToolOutput {
        tool_with(code_cfg)
            .run_with_emit(&json!({ "code": code }), &ctx(&scratch()), &mut |_| {})
            .unwrap()
    }

    #[test]
    fn returns_json_and_collects_console() {
        let out = run_code(
            r#"console.log("hi"); return {a: 1 + 1};"#,
            CodeConfig::default(),
        );
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "{\"a\":2}\n\n# console\nhi");
    }

    #[test]
    fn poisons_date_and_random() {
        // FR-2: accidental nondeterminism must be loud. Both getters throw.
        for expr in ["Date.now()", "Math.random()"] {
            let out = run_code(&format!("return {expr};"), CodeConfig::default());
            assert!(out.is_error, "{expr} must throw");
            assert!(
                out.content.contains("not available in run_code"),
                "{expr}: {}",
                out.content
            );
        }
    }

    // ── FR-2 precheck (finetune_support A-3/A-4/A-5) ───────────────────────

    fn precheck_cfg() -> CodeConfig {
        CodeConfig {
            precheck: true,
            ..CodeConfig::default()
        }
    }

    /// A-3 环境语法类：is_error + 附录 A-1 文案逐字一致。
    #[test]
    fn precheck_flags_async_with_exact_copy() {
        let out = run_code(
            "let x = 1;\n\nasync function f() {}\nreturn x;",
            precheck_cfg(),
        );
        assert!(out.is_error);
        assert_eq!(
            out.content,
            "run_code precheck: 检出环境不支持的语法 \"async\"（第 3 行）——本沙箱是同步环境：没有 async/await/Promise，没有 setTimeout 等 timer，没有 import/fetch。请改同步写法：顺序执行；批量数据在一次执行内用循环 + tools.call 逐次处理。代码未执行，无副作用；修正后重发完整代码。"
        );
    }

    /// A-3 确定性类：附录 A-2 文案逐字一致。
    #[test]
    fn precheck_flags_date_now_with_exact_copy() {
        let out = run_code("let t = Date.now();\nreturn t;", precheck_cfg());
        assert!(out.is_error);
        assert_eq!(
            out.content,
            "run_code precheck: 检出确定性违规 \"Date.now\"（第 1 行）——确定性沙箱不提供 Date.now / Math.random（调用即在运行时抛错，不是返回固定值的桩）。需要时间或随机值时，改用数据记录自带的字段或代码内常量。代码未执行，无副作用；修正后重发完整代码。"
        );
    }

    /// A-3 保留工具类：附录 A-3 文案逐字一致（FD-9 修正后集合——task 在
    /// {run_code, task, todo} 内，__tools_list/__budget 不在）。
    #[test]
    fn precheck_flags_reserved_tool_with_exact_copy() {
        let out = run_code("return tools.call(\"task\", {});", precheck_cfg());
        assert!(out.is_error);
        assert_eq!(
            out.content,
            "run_code precheck: 检出 tools.call(\"task\")（第 1 行）——\"task\" 在沙箱内不可达（设计 D11：防自递归与嵌套成本）。沙箱内只能调用 tools.list() 列出的工具；\"task\" 的调用请移出沙箱、由主循环直接发起。代码未执行，无副作用；修正后重发完整代码。"
        );
    }

    /// FD-11：一次拒答报告全部命中，类间固定序（环境语法 → 确定性 → 保留
    /// 工具），类内按文档序带行号。
    #[test]
    fn precheck_reports_all_classes_in_fixed_order() {
        let out = run_code("let t = Math.random();\nawait f();", precheck_cfg());
        assert!(out.is_error);
        let env = out.content.find("检出环境不支持的语法").unwrap();
        let det = out.content.find("检出确定性违规").unwrap();
        assert!(env < det, "{}", out.content);
        assert!(
            out.content.contains("\"await\"（第 2 行）"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("\"Math.random\"（第 1 行）"),
            "{}",
            out.content
        );
    }

    /// FD-11 封顶：每类列前 20 处，溢出写「等 N 处」（N = 该类总命中数）。
    #[test]
    fn precheck_caps_hits_at_20_with_total() {
        let code = (0..25)
            .map(|i| format!("await f{i}();"))
            .collect::<Vec<_>>()
            .join("\n");
        let msg = precheck(&code).expect("25 awaits must be flagged");
        assert_eq!(msg.matches("（第 ").count(), 20, "{msg}");
        assert!(msg.contains("等 25 处"), "{msg}");
    }

    /// A-3 排序证明 + 零副作用：registry 必坏（白名单含未知工具名）的实例
    /// 上，预检文案仍先于 registry 构建错误返回——构造性证明拒答先于 eval
    /// （wasm 引擎零调用）；scratch 目录零文件（无副作用）。
    #[test]
    fn precheck_fires_before_registry_and_leaves_no_side_effects() {
        let wd = scratch();
        let tool = tool_with_tools(precheck_cfg(), r#"["no_such_tool_xyz"]"#);
        let out = tool
            .run_with_emit(
                &json!({"code": "async function f() {}"}),
                &ctx(&wd),
                &mut |_| {},
            )
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content
                .starts_with("run_code precheck: 检出环境不支持的语法"),
            "{}",
            out.content
        );
        // Sanity: the SAME tool on clean code does hit the broken registry —
        // the rejection above provably never reached eval.
        let clean = tool
            .run_with_emit(&json!({"code": "return 1;"}), &ctx(&wd), &mut |_| {})
            .unwrap();
        assert!(clean.is_error);
        assert!(
            clean.content.contains("registry build failed"),
            "{}",
            clean.content
        );
        assert_eq!(std::fs::read_dir(&wd).unwrap().count(), 0);
    }

    /// A-3 对照：字符串/注释里的 import、await、Date.now 是文本不是语法
    /// （真 lexer 非 substring，需求 v0.2 修订三）。
    #[test]
    fn precheck_ignores_strings_and_comments() {
        let out = run_code(
            "let s = \"no import here\"; // await Date.now in comment\nreturn s;",
            precheck_cfg(),
        );
        assert!(!out.is_error, "{}", out.content);
    }

    /// A-4 诚实边界：混淆写法过预检（预检是效率特性不是安全机制），运行时
    /// 确定性桩照样抛错——两层分工钉死。
    #[test]
    fn precheck_passes_obfuscated_but_runtime_stub_throws() {
        let out = run_code(r#"return globalThis["Date"]["now"]();"#, precheck_cfg());
        assert!(out.is_error, "{}", out.content);
        assert!(
            !out.content.starts_with("run_code precheck:"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("not available in run_code"),
            "{}",
            out.content
        );
    }

    /// A-5：已声明的名字是模型有意自定义（FD-10 声明豁免）；FD-9——
    /// __budget/__tools_list 是可工作的内省拦截，不在检出集合内。
    #[test]
    fn precheck_accepts_declared_fetch_and_budget_introspection() {
        let out = run_code(
            "function fetch(x) { return x; }\nreturn fetch(41) + 1;",
            precheck_cfg(),
        );
        assert!(!out.is_error, "{}", out.content);
        let out = run_code(
            "return typeof tools.call(\"__budget\", {});",
            precheck_cfg(),
        );
        assert!(!out.is_error, "{}", out.content);
    }

    /// A-5 语料：skill md 的全部 js 示例逐块过预检不拦——技能文档教给模型
    /// 的写法必须全部通过预检。
    #[test]
    fn precheck_accepts_all_skill_md_examples() {
        let mut in_block = false;
        let mut block = String::new();
        let mut blocks: Vec<String> = Vec::new();
        for line in RUN_CODE_SKILL_MD.lines() {
            let t = line.trim();
            if !in_block && t.starts_with("```js") {
                in_block = true;
            } else if in_block && t == "```" {
                blocks.push(std::mem::take(&mut block));
                in_block = false;
            } else if in_block {
                block.push_str(line);
                block.push('\n');
            }
        }
        assert!(!blocks.is_empty(), "skill md has no js blocks?");
        for b in &blocks {
            assert!(
                precheck(b).is_none(),
                "skill md example flagged:\n{b}\n→ {}",
                precheck(b).unwrap_or_default()
            );
        }
    }

    #[test]
    fn js_exception_reports_message_and_stack() {
        let out = run_code(r#"throw new Error("boom")"#, CodeConfig::default());
        assert!(out.is_error);
        assert!(
            out.content.contains("[run_code] JS error:"),
            "{}",
            out.content
        );
        assert!(out.content.contains("boom"), "{}", out.content);
    }

    #[test]
    fn tools_list_names_the_registry() {
        // Registry = cfg whitelist minus subagent-inheritable exclusions:
        // read_file/list_dir stay, and no reserved/nested tools appear.
        let out = run_code(
            "return tools.list().map(t => t.name).sort();",
            CodeConfig::default(),
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("read_file"), "{}", out.content);
        assert!(out.content.contains("list_dir"), "{}", out.content);
        assert!(!out.content.contains("run_code"), "{}", out.content);
        assert!(!out.content.contains("__"), "{}", out.content);
    }

    #[test]
    fn host_tool_error_throws_in_guest() {
        // A tool-level error (missing file) crosses the envelope as a JS
        // exception the guest can catch — the dual-envelope contract (§5.1).
        let out = run_code(
            r#"try { tools.call("read_file", {path: "definitely-missing.txt"}); return "no throw"; }
               catch (e) { return "caught: " + (e instanceof Error); }"#,
            CodeConfig::default(),
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("caught: true"), "{}", out.content);
    }

    #[test]
    fn d4_raw_string_passthrough() {
        // read_file content is NOT JSON — D4 says the guest still receives it
        // as a plain string, not a parse failure.
        let dir = scratch();
        std::fs::write(dir.join("plain.txt"), "not json at all\n").unwrap();
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"const r = tools.call("read_file", {path: "plain.txt"});
                              return typeof r;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("\"string\""), "{}", out.content);
    }

    #[test]
    fn guest_tool_output_uses_data_scale_budget_not_display_cap() {
        // Real-journey A/B finding: guest_ctx used to inherit the outer ctx's
        // display-scale max_output_chars (1000 here, limits.max_output_chars in
        // production), so a bulk read inside the sandbox got one page no
        // matter the requested limit — the code feature's whole point
        // (processing data OUT of the model's context) was unreachable.
        let dir = scratch();
        let filler = "x".repeat(100);
        let body = (0..30)
            .map(|i| format!("{i:03} {filler}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join("bulk.txt"), &body).unwrap();
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"const t = tools.call("read_file", {path: "bulk.txt"});
return t.length;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // ~3.3k chars of content — far past the outer ctx's 1000-char cap,
        // comfortably under the guest's data-scale budget.
        let len: usize = out.content.trim().parse().expect(out.content.as_str());
        assert!(len > 3000, "guest saw only {len} chars of a 3.3k file");
    }

    #[test]
    fn inner_tool_events_reach_the_emit_sink() {
        let dir = scratch();
        std::fs::write(dir.join("ev.txt"), "payload\n").unwrap();
        let mut events: Vec<AgentEvent> = Vec::new();
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"return tools.call("read_file", {path: "ev.txt"});"#}),
                &ctx(&dir),
                &mut |e| events.push(e),
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("payload"), "{}", out.content);
        // Inner call = ToolCall + ToolResult, in that order (H1: run_code
        // emits the result itself — the outer loop never sees these blocks).
        let pos = |needle: &str, is_result: bool| {
            events.iter().position(|e| match e {
                AgentEvent::ToolCall { name, .. } => !is_result && name == needle,
                AgentEvent::ToolResult { name, .. } => is_result && name == needle,
                _ => false,
            })
        };
        let call = pos("read_file", false).expect("inner ToolCall event");
        let result = pos("read_file", true).expect("inner ToolResult event");
        assert!(call < result, "ToolCall must precede ToolResult");
    }

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
    fn missing_code_field_is_tool_error() {
        let out = tool_with(CodeConfig::default())
            .run_with_emit(&json!({}), &ctx(&scratch()), &mut |_| {})
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("missing string field 'code'"));
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

    // ── M2: interception layer + audit + call limit (design §7/§8.4) ─────────

    /// glob semantics vectors (§13-6 + review 低-3): `*` stays in-segment,
    /// whole-`**` segments cross (and match ZERO segments), a no-`/` pattern
    /// matches only the root's first segment, nested `.aws` needs its own
    /// pattern.
    #[test]
    fn glob_semantics_vectors() {
        let m = |pat: &str, path: &str| deny_match(&[pat.to_string()], path);
        // No-`/` patterns: root FIRST segment only.
        assert!(m(".env*", ".env"));
        assert!(m(".env*", ".env.local"));
        assert!(!m(".env*", "a/.env"), "no-/ pattern must not match nested");
        assert!(m("*.pem", "server.pem"));
        assert!(!m("*.pem", "keys/server.pem"));
        // `**/` matches zero or more segments — root included.
        assert!(m("**/.env*", ".env"));
        assert!(m("**/.env*", "a/.env"));
        assert!(m("**/.env*", "a/b/.env.local"));
        assert!(m("**/id_rsa*", "id_rsa"));
        assert!(m("**/id_rsa*", ".ssh/id_rsa.pub"));
        // `.aws` at root vs nested — both default entries matter.
        assert!(m(".aws/**", ".aws/config"));
        assert!(!m(".aws/**", "x/.aws/config"), "nested needs ** prefix");
        assert!(m("**/.aws/**", "x/.aws/config"));
        assert!(m("**/.aws/**", ".aws"), "`**` tail matches zero segments");
        // The directory ROOT itself (bare path, no trailing /) — external
        // review P2 asked whether `.aws/**`-style rules leak it to `list_dir`;
        // here the `**` tail matches zero segments, so both bare roots are
        // covered by the same entries (no gitignore-style "contents only").
        assert!(m(".aws/**", ".aws"), "bare root dir");
        assert!(m("**/.aws/**", "x/.aws"), "bare nested dir root");
        // `.git` as dir-tree and as file (linked worktrees) — both entries.
        assert!(m("**/.git/**", ".git/hooks/pre-commit"));
        assert!(m("**/.git/**", "sub/.git/config"));
        assert!(m("**/.git", "sub/.git"));
        // Mid-path `**`.
        assert!(m("a/**/b", "a/b"));
        assert!(m("a/**/b", "a/x/y/b"));
        assert!(!m("a/**/b", "a/c"));
        // Case folding on case-insensitive volumes (macOS/Windows builds).
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        assert!(m(".env*", ".ENV"), "case-insensitive volumes must fold");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert!(!m(".env*", ".ENV"), "case-sensitive volumes stay exact");
    }

    /// A-7: deny_paths hits across oblique spellings — the workdir-relative
    /// normalization kills `./` and `a/../` bypasses before matching.
    #[test]
    fn deny_paths_block_env_reads_across_spellings() {
        let dir = scratch();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"
                    const spellings = [".env", "./.env", "sub/../.env"];
                    const results = [];
                    for (const p of spellings) {
                        try { tools.call("read_file", {path: p}); results.push(p + ":ran"); }
                        catch (e) { results.push(p + ":" + (String(e.message).indexOf("deny_paths") >= 0 ? "denied" : "other")); }
                    }
                    return results;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains(".env:denied")
                && out.content.contains("./.env:denied")
                && out.content.contains("sub/../.env:denied"),
            "every spelling must be denied: {}",
            out.content
        );
        // The denial is audited: 3 traced calls, all ok:false.
        let meta = out.meta.expect("audit meta");
        assert_eq!(meta["call_count"], 3, "{meta}");
        assert!(
            meta["calls"]
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c["ok"] == false),
            "{meta}"
        );
    }

    /// 外部评审 P2 跟进：默认 deny_paths 必须拦住密钥目录**本身**（裸目录根，
    /// 无尾 `/`），不止其内容 —— `list_dir` on `.aws`（workdir 根）与 `x/.aws`
    /// （嵌套）都要被拒。本实现的尾段 `**` 跨零段（glob_semantics_vectors 钉住），
    /// `.aws/**` / `**/.aws/**` 直接命中裸目录路径，无需补 `**/.aws` 类条目。
    #[test]
    fn deny_paths_block_listing_secret_dir_root() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join(".aws")).unwrap();
        std::fs::create_dir_all(dir.join("x/.aws")).unwrap();
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"
                    const results = [];
                    for (const p of [".aws", "./.aws", "x/.aws", "x/../.aws"]) {
                        try { tools.call("list_dir", {path: p}); results.push(p + ":ran"); }
                        catch (e) { results.push(p + ":" + (String(e.message).indexOf("deny_paths") >= 0 ? "denied" : "other")); }
                    }
                    return results;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        for want in [
            ".aws:denied",
            "./.aws:denied",
            "x/.aws:denied",
            "x/../.aws:denied",
        ] {
            assert!(
                out.content.contains(want),
                "listing the secret dir root must be denied: {want}\n{}",
                out.content
            );
        }
    }

    /// v2 review — `tools.budget()`: reports the live limits and costs
    /// neither a call nor quota (same reserved-name class as `__tools_list`).
    #[test]
    fn tools_budget_reports_limits_and_costs_nothing() {
        let dir = scratch();
        std::fs::write(dir.join("a.txt"), "x\n").unwrap();
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"
                    const before = tools.budget();
                    tools.call("read_file", {path: "a.txt"});
                    const after = tools.budget();
                    return {
                        before_used: before.calls_used, calls_used: after.calls_used,
                        calls_left: after.calls_left, max: after.max_tool_calls,
                        wall: typeof after.wall_left_ms, bound_by: after.wall_bound_by,
                        fuel: typeof after.fuel_left, tokens: after.tokens_left === null
                    };"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let ok = serde_json::from_str::<Value>(&out.content).unwrap_or(json!(null));
        assert_eq!(ok["before_used"], json!(0), "{}", out.content);
        assert_eq!(ok["calls_used"], json!(1), "{}", out.content);
        assert_eq!(ok["calls_left"], json!(199), "{}", out.content);
        assert_eq!(ok["max"], json!(200), "{}", out.content);
        assert_eq!(ok["wall"], json!("number"), "{}", out.content);
        assert_eq!(
            ok["bound_by"],
            json!("code.timeout_secs"),
            "{}",
            out.content
        );
        assert_eq!(ok["fuel"], json!("number"), "{}", out.content);
        // No run-level token gate installed (max_total_tokens=0): null, same
        // nullable convention as an absent wall clock — NOT a lying 0.
        assert_eq!(ok["tokens"], json!(true), "{}", out.content);
        // Only the ONE real read is audited — budget queries are free.
        let meta = out.meta.expect("audit meta");
        assert_eq!(meta["call_count"], 1, "{meta}");
    }

    /// v0.7 — the run-level token gate reaches `tools.budget()` as
    /// `tokens_left` (cap − live spend, saturating): the guest ctx CLONES the
    /// outer gate, so a query mid-execution sees what the run loop billed so
    /// far, and an over-cap counter reports 0 (never a wrapped huge number).
    #[test]
    fn tools_budget_reports_run_token_gate() {
        let dir = scratch();
        let g = ToolCtx {
            budget: Some(crate::tools::BudgetGate {
                used: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(120)),
                cap: 150,
            }),
            ..ctx(&dir)
        };
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"
                    const b = tools.budget();
                    return { left: b.tokens_left };"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let ok = serde_json::from_str::<Value>(&out.content).unwrap_or(json!(null));
        assert_eq!(ok["left"], json!(30), "{}", out.content);
        // Live read: billing more on the host side between two guest queries
        // moves the number (the used counter is Arc-shared, not snapshotted).
        g.budget
            .as_ref()
            .unwrap()
            .used
            .fetch_add(45, std::sync::atomic::Ordering::Relaxed);
        let out2 = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"return { left: tools.budget().tokens_left };"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        assert!(!out2.is_error, "{}", out2.content);
        let ok2 = serde_json::from_str::<Value>(&out2.content).unwrap_or(json!(null));
        assert_eq!(ok2["left"], json!(0), "{}", out2.content);
    }

    /// External-review round: malformed guest arguments are a structured
    /// `host` error, not a silent `{}` degradation. The shipped glue always
    /// sends valid JSON (stringify or the literal `{}` for undefined), so this
    /// path only exists for a guest driving the raw wasm import with garbage
    /// bytes — the helper is the unit under test (the full path needs a
    /// hand-crafted wasm caller the sandbox never exposes to JS).
    #[test]
    fn malformed_guest_args_error_instead_of_empty_object() {
        assert_eq!(parse_guest_args("{}").unwrap(), json!({}));
        assert_eq!(
            parse_guest_args(r#"{"path": "a.txt"}"#).unwrap(),
            json!({"path": "a.txt"})
        );
        // The glue's "no args" literal and non-object JSON values are all
        // valid — behavior unchanged for every reachable guest input.
        assert_eq!(parse_guest_args("5").unwrap(), json!(5));
        assert_eq!(parse_guest_args("\"str\"").unwrap(), json!("str"));
        // Garbage bytes → Err naming the parse failure (host kind, retryable
        // false — a guest code bug, not a transient condition).
        for garbage in ["undefined", "{oops", ""] {
            let e = parse_guest_args(garbage).expect_err("garbage must fail, not become {}");
            assert!(e.contains("not valid JSON"), "{e}");
        }
    }

    /// v2 review — structured error fields: `e.kind` / `e.tool` /
    /// `e.retryable` on every host-classified failure, message unchanged.
    /// Plain tool errors carry kind "tool" and deliberately NO retryable
    /// (the host can't classify a tool's own error — both hard values
    /// mislead).
    #[test]
    fn structured_error_fields_surface_in_guest() {
        let dir = scratch();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"
                    function probe(name, args) {
                        try { tools.call(name, args); return name + ":ran"; }
                        catch (e) {
                            return name + ":" + e.kind + ":" + e.tool + ":" +
                                   (e.retryable === undefined ? "absent" : e.retryable) + ":" +
                                   (typeof e.message === "string" && e.message.length > 0 ? "msg" : "nomsg");
                        }
                    }
                    return [
                        probe("read_file", {path: ".env"}),
                        probe("definitely-not-a-tool", {}),
                        probe("read_file", {path: "missing.txt"}),
                        probe("copy_bytes", {src: ".env", dst: "out.env"})
                    ];"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // deny_paths → denied, definitive, tool name attached, message intact
        assert!(
            out.content.contains("read_file:denied:read_file:false:msg"),
            "{}",
            out.content
        );
        // unknown name → unknown_tool (definitive)
        assert!(
            out.content
                .contains("definitely-not-a-tool:unknown_tool:definitely-not-a-tool:false:msg"),
            "{}",
            out.content
        );
        // plain tool failure → kind "tool", retryable ABSENT
        assert!(
            out.content.contains("read_file:tool:read_file:absent:msg"),
            "{}",
            out.content
        );
        // copy interception shares the denied class
        assert!(
            out.content
                .contains("copy_bytes:denied:copy_bytes:false:msg"),
            "{}",
            out.content
        );
    }

    /// v2 review — `copy_bytes`: host-streamed byte-exact copy (binary /
    /// invalid-UTF-8 survive without touching a JS string), full policy
    /// surface (fs_write_dirs, deny src, deny dst, outside-workdir dst), and
    /// a REAL audited call.
    #[test]
    fn copy_bytes_is_byte_exact_and_policy_judged() {
        let dir = scratch();
        // 0x00 / 0xFF / invalid-UTF-8 continuation / valid multibyte — bytes
        // that must not silently round-trip through a JS string.
        let blob: Vec<u8> = vec![0x00, 0xFF, 0xC3, 0x28, 0xE4, 0xB8, 0xAD, 0x0A, 0x00];
        std::fs::write(dir.join("blob.bin"), &blob).unwrap();
        std::fs::create_dir_all(dir.join(".aws")).unwrap();
        std::fs::write(dir.join(".aws/creds"), b"AKIA...").unwrap();
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let cc = CodeConfig {
            fs_write_dirs: vec!["out".into()],
            ..CodeConfig::default()
        };
        let out = tool_with(cc)
            .run_with_emit(
                &json!({"code": r#"
                    const r = [];
                    const c = tools.call("copy_bytes", {src: "blob.bin", dst: "out/blob.copy"});
                    r.push("copied:" + c.copied_bytes);
                    const back = tools.call("read_file", {path: "out/blob.copy", raw: true});
                    r.push("bytes:" + back.bytes_read);
                    try { tools.call("copy_bytes", {src: "blob.bin", dst: "leak.bin"}); r.push("leak:ran"); }
                    catch (e) { r.push("leak:" + e.kind); }
                    try { tools.call("copy_bytes", {src: ".aws/creds", dst: "out/x"}); r.push("aws:ran"); }
                    catch (e) { r.push("aws:" + e.kind); }
                    try { tools.call("copy_bytes", {src: "blob.bin", dst: "out/.env"}); r.push("envdst:ran"); }
                    catch (e) { r.push("envdst:" + e.kind); }
                    try { tools.call("copy_bytes", {src: "blob.bin", dst: "../escape.bin"}); r.push("esc:ran"); }
                    catch (e) { r.push("esc:" + e.kind); }
                    return r;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("copied:9"), "{}", out.content);
        assert!(out.content.contains("bytes:9"), "{}", out.content);
        // Host-side byte equality is the ground truth, not the guest's say-so.
        assert_eq!(std::fs::read(dir.join("out/blob.copy")).unwrap(), blob);
        for want in ["leak:denied", "aws:denied", "envdst:denied", "esc:denied"] {
            assert!(
                out.content.contains(want),
                "policy must deny this copy: {want}\n{}",
                out.content
            );
        }
        // The escape attempt left nothing behind.
        assert!(!dir.parent().unwrap().join("escape.bin").exists());
        // 1 ok copy + 1 read-back + 4 refused = 6 audited calls.
        let meta = out.meta.expect("audit meta");
        assert_eq!(meta["call_count"], 6, "{meta}");
    }

    /// v2 review — extra_tools reach the guest registry ONLY on the tool's
    /// own `available_to_code()` opt-in (`CallbackTool::with_code_access`);
    /// default-closed keeps "usable top-level, invisible in code" for tools
    /// that didn't opt in. `copy_bytes` (guest-only primitive) is always
    /// present.
    #[test]
    fn extra_tools_join_guest_registry_only_on_opt_in() {
        use crate::tools::CallbackTool;
        let dir = scratch();
        let mk = |name: &'static str, code: bool| {
            let t = CallbackTool::new(
                ToolDef {
                    name: name.into(),
                    description: "host callback".into(),
                    schema: json!({"type": "object", "properties": {
                        "who": {"type": "string"}
                    }}),
                },
                |input: &Value, _ctx: &ToolCtx| {
                    Ok(ToolOutput::text(format!(
                        "hi:{}",
                        input["who"].as_str().unwrap_or("?")
                    )))
                },
            );
            let b: Box<dyn Tool> = if code {
                Box::new(t.with_code_access())
            } else {
                Box::new(t)
            };
            Arc::new(b)
        };
        let extras: Vec<Arc<Box<dyn Tool>>> = vec![mk("host_open", true), mk("host_closed", false)];
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":["read_file"],"code":{"enabled":true}}"#,
        )
        .unwrap();
        let tool = RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(CodeConfig::default())),
            &reqwest::blocking::Client::new(),
            "k",
            vec![],
            extras,
        );
        let out = tool
            .run_with_emit(
                &json!({"code": r#"
                    const names = tools.list().map(function (t) { return t.name; });
                    var res = "open:" + (names.indexOf("host_open") >= 0) +
                              ",closed:" + (names.indexOf("host_closed") >= 0) +
                              ",copy:" + (names.indexOf("copy_bytes") >= 0);
                    res += ",call:" + tools.call("host_open", {who: "guest"});
                    try { tools.call("host_closed", {who: "x"}); res += ",closedcall:ran"; }
                    catch (e) { res += ",closedcall:" + e.kind; }
                    return res;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("open:true,closed:false,copy:true"),
            "{}",
            out.content
        );
        assert!(out.content.contains("call:hi:guest"), "{}", out.content);
        assert!(
            out.content.contains("closedcall:unknown_tool"),
            "{}",
            out.content
        );
    }

    /// Dangling-symlink setup; false where the platform can't test it
    /// (non-unix would need symlink privileges we don't assume). Runtime
    /// bool, not `#[cfg]` + `return` — the latter makes the body below
    /// unreachable and fails `clippy --features code` on Windows with
    /// `-D warnings` (`unreachable_statement`), a gate Linux CI can't see.
    #[cfg(unix)]
    fn create_dangling_symlink(target: &str, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[cfg(not(unix))]
    fn create_dangling_symlink(_target: &str, _link: &Path) -> bool {
        false
    }

    /// 中-1: a dangling symlink write target is refused BEFORE matching —
    /// `ln -s .env bak` then "write bak" must not land on `.env`.
    #[test]
    fn dangling_symlink_write_target_rejected() {
        let dir = scratch();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        if !create_dangling_symlink(".env", &dir.join("bak")) {
            eprintln!("symlink test skipped on this platform");
            return;
        }
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"
                    try { tools.call("write_file", {path: "bak", content: "pwned"}); return "wrote"; }
                    catch (e) { return e.message; }"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("symlink"),
            "the refusal must name the symlink: {}",
            out.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(".env")).unwrap(),
            "SECRET=1\n",
            "the write must NOT have landed on .env"
        );
    }

    /// 中-2: with restrict_to_workdir=false, a READ outside the workdir
    /// passes (deny_paths cannot see it — documented boundary), while a
    /// WRITE outside is always refused (guest writes are bounded by the
    /// workdir; fs_write_dirs is a permission list, not a sandbox mirror).
    #[test]
    fn outside_workdir_read_passes_write_denied() {
        let outer = std::env::temp_dir().join(format!("ta-run-code-out-{}", std::process::id()));
        std::fs::create_dir_all(&outer).unwrap();
        let dir = outer.join("inside");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(outer.join("readable.txt"), "outside-content\n").unwrap();
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"
                    const r = tools.call("read_file", {path: "../readable.txt"});
                    let w;
                    try { tools.call("write_file", {path: "../written.txt", content: "x"}); w = "wrote"; }
                    catch (e) { w = e.message; }
                    return {read: r.slice(-10), write: w.slice(0, 60)};"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("e-content"),
            "outside read must pass (the guest shows read_file's tail): {}",
            out.content
        );
        assert!(
            out.content.contains("limited to the working"),
            "outside write must be refused: {}",
            out.content
        );
        assert!(
            !outer.join("written.txt").exists(),
            "the refused write must not have landed"
        );
    }

    /// fs_write_dirs: when non-empty, guest writes must land inside one of
    /// the listed dirs (workdir-relative, prefix containment).
    #[test]
    fn fs_write_dirs_limit_guest_writes() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let cfg = CodeConfig {
            fs_write_dirs: vec!["out".into()],
            ..CodeConfig::default()
        };
        let out = tool_with_tools(cfg, r#"["read_file","write_file"]"#)
            .run_with_emit(
                &json!({"code": r#"
                    let inside, root;
                    try { tools.call("write_file", {path: "out/a.txt", content: "ok"}); inside = "ok"; }
                    catch (e) { inside = "err:" + e.message.slice(0, 30); }
                    try { tools.call("write_file", {path: "root.txt", content: "x"}); root = "wrote"; }
                    catch (e) { root = "denied"; }
                    return {inside, root};"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("\"inside\":\"ok\"")
                && out.content.contains("\"root\":\"denied\""),
            "{}",
            out.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("out/a.txt")).unwrap(),
            "ok"
        );
        assert!(!dir.join("root.txt").exists());
    }

    /// Test 7 (D11): the reserved trio is unreachable from the guest —
    /// run_code (no self-recursion), task (no cost bomb), todo (parent's).
    #[test]
    fn guest_cannot_reach_run_code_task_todo() {
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":["read_file","task","todo"]}"#,
        )
        .unwrap();
        let out = RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(CodeConfig::default())),
            &reqwest::blocking::Client::new(),
            "k",
            vec![],
            vec![],
        )
        .run_with_emit(
            &json!({"code": r#"
                const names = ["run_code", "task", "todo"];
                const out = {};
                for (const n of names) {
                    try { tools.call(n, {}); out[n] = "RAN"; }
                    catch (e) { out[n] = String(e.message).indexOf("unknown tool") >= 0 ? "unknown" : "other"; }
                }
                return out;"#}),
            &ctx(&scratch()),
            &mut |_| {},
        )
        .unwrap();
        assert!(!out.is_error, "{}", out.content);
        for n in ["run_code", "task", "todo"] {
            assert!(
                out.content.contains(&format!("\"{n}\":\"unknown\"")),
                "{n} must be unknown to the guest: {}",
                out.content
            );
        }
    }

    /// Test 8 (D12): a tool output that itself carries `__ok` keys cannot spoof
    /// the envelope — the content arrives as a SUCCESS value the guest parses
    /// itself (read_file's line prefix keeps it a raw string per D4; the
    /// F-2 strip + JSON.parse is in-guest), never as a failure signal.
    #[test]
    fn envelope_not_spoofed_by_tool_output() {
        let dir = scratch();
        std::fs::write(dir.join("trap.json"), r#"{"__ok":false,"error":"spoof"}"#).unwrap();
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"
                    let thrown = false;
                    let v = null;
                    try {
                        const r = tools.call("read_file", {path: "trap.json"});
                        v = JSON.parse(r.slice(r.indexOf("\t") + 1));
                    } catch (e) { thrown = true; }
                    return {thrown, ok: v && v.__ok, err: v && v.error};"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("\"thrown\":false")
                && out.content.contains("\"ok\":false")
                && out.content.contains("\"err\":\"spoof\""),
            "spoof payload must arrive as plain success data: {}",
            out.content
        );
    }

    /// A-8: shell is deliberately OUT of the interception layer's scope —
    /// `cat .env` passes straight through (deny_paths is an anti-misread
    /// guard, not a security boundary; the hostile-guest remedies are
    /// restrict_to_workdir / hooks Deny / dropping shell).
    #[test]
    fn shell_bypasses_deny_paths_documented_boundary() {
        let dir = scratch();
        std::fs::write(dir.join(".env"), "SHELLBYPASS=42\n").unwrap();
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":["shell"]}"#,
        )
        .unwrap();
        let out = RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(CodeConfig::default())),
            &reqwest::blocking::Client::new(),
            "k",
            vec![],
            vec![],
        )
        .run_with_emit(
            &json!({"code": r#"return tools.call("shell", {command: "cat .env"}).trim();"#}),
            &ctx(&dir),
            &mut |_| {},
        )
        .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("SHELLBYPASS=42"),
            "shell passes .env through by design: {}",
            out.content
        );
    }

    /// max_tool_calls: the first N calls run, N+1 is refused AND latches —
    /// later calls fail fast without reaching any tool.
    #[test]
    fn call_limit_halts_and_audits() {
        let dir = scratch();
        for i in 0..5 {
            std::fs::write(dir.join(format!("f{i}.txt")), "x\n").unwrap();
        }
        let cfg = CodeConfig {
            max_tool_calls: 2,
            ..CodeConfig::default()
        };
        let out = tool_with(cfg)
            .run_with_emit(
                &json!({"code": r#"
                    let ok = 0, limited = 0;
                    for (let i = 0; i < 5; i++) {
                        try { tools.call("read_file", {path: "f" + i + ".txt"}); ok++; }
                        catch (e) { if (String(e.message).indexOf("tool call limit") >= 0) limited++; }
                    }
                    return {ok, limited};"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("\"ok\":2") && out.content.contains("\"limited\":3"),
            "2 calls run, 3 refused: {}",
            out.content
        );
        // The audit reports count against limit — and only the REAL calls.
        let meta = out.meta.expect("audit meta");
        assert_eq!(meta["call_count"], 2, "{meta}");
        assert_eq!(meta["call_limit"], 2, "{meta}");
    }

    /// Test 10: `list_entry` caps descriptions at 200 chars (char-safe).
    #[test]
    fn list_entry_truncates_long_descriptions() {
        let def = ToolDef {
            name: "verbose".into(),
            description: "x".repeat(350),
            schema: json!({}),
        };
        let e = list_entry(&def);
        assert_eq!(e["description"].as_str().unwrap().chars().count(), 200);
        let short = ToolDef {
            name: "s".into(),
            description: "short one".into(),
            schema: json!({}),
        };
        assert_eq!(list_entry(&short)["description"], "short one");
    }

    /// A-3 (unit half): hooks see `__code__` as the sub-agent marker, a
    /// Modify swaps the EXECUTED path while the event/audit trail keeps the
    /// original, and the dual hashes prove both inputs (§8.4 中-3/中-5).
    #[test]
    fn hooks_marker_modify_and_dual_hash() {
        use crate::hooks::{Hooks, ToolDecision};
        use std::sync::Mutex;

        struct SwapPathHooks {
            seen: Mutex<Vec<String>>,
        }
        impl Hooks for SwapPathHooks {
            fn before_tool(&self, tool: &str, _input: &Value, c: &ToolCtx) -> ToolDecision {
                self.seen.lock().unwrap().push(format!(
                    "before:{tool}:{}",
                    c.subagent.clone().unwrap_or_default()
                ));
                if tool == "read_file" {
                    ToolDecision::Modify(json!({"path": "real-target.txt"}))
                } else {
                    ToolDecision::Allow
                }
            }
        }

        let dir = scratch();
        std::fs::write(dir.join("asked-for.txt"), "ORIGINAL\n").unwrap();
        std::fs::write(dir.join("real-target.txt"), "MODIFIED\n").unwrap();
        let hooks = Arc::new(SwapPathHooks {
            seen: Mutex::new(Vec::new()),
        });
        let mut c = ctx(&dir);
        c.hooks = Some(hooks.clone());

        let mut events: Vec<AgentEvent> = Vec::new();
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"return tools.call("read_file", {path: "asked-for.txt"}).trim();"#}),
                &c,
                &mut |e| events.push(e),
            )
            .unwrap();
        // The guest ran the MODIFIED input (FR-3.3 semantics), …
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("MODIFIED") && !out.content.contains("ORIGINAL"),
            "execution must use the Modify replacement: {}",
            out.content
        );
        // …the event audit keeps the ORIGINAL input, …
        let call = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolCall { name, input } if name == "read_file" => Some(input.clone()),
                _ => None,
            })
            .expect("inner ToolCall event");
        assert_eq!(call["path"], "asked-for.txt");
        // …and the hooks layer saw the `__code__` marker.
        assert_eq!(
            hooks.seen.lock().unwrap()[0],
            "before:read_file:__code__",
            "gate must tag guest-originated calls"
        );
        // Dual hashes: original vs. effective differ (中-3).
        let meta = out.meta.expect("audit meta");
        let c0 = &meta["calls"][0];
        assert!(
            c0["in_hash"].as_str().unwrap().starts_with("sha256:"),
            "{meta}"
        );
        let eff = c0["in_hash_eff"].as_str().expect("effective hash present");
        assert_ne!(eff, c0["in_hash"].as_str().unwrap(), "{meta}");
        // …and an un-Modified run omits the duplicate.
        let out2 = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"return tools.call("read_file", {path: "asked-for.txt"}).trim();"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(
            out2.meta.as_ref().unwrap()["calls"][0]
                .get("in_hash_eff")
                .is_none(),
            "no Modify → no redundant second hash"
        );
    }

    /// fuel accounting lands in the audit meta even on a clean run.
    #[test]
    fn audit_meta_reports_fuel_used() {
        let out = run_code("return 1 + 1;", CodeConfig::default());
        let meta = out.meta.expect("audit meta");
        assert!(meta["fuel_used"].as_u64().unwrap_or(0) > 0, "{meta}");
        assert_eq!(meta["call_count"], 0, "no tool calls were made");
    }

    /// Sec-review M-1: a symlinked INTERMEDIATE component (`out -> somewhere
    /// outside`) must not let a write escape the workdir — the lexical form
    /// says "inside", the open() follows the link outside.
    #[cfg(unix)]
    #[test]
    fn write_through_intermediate_symlink_rejected() {
        let dir = scratch();
        let outside =
            std::env::temp_dir().join(format!("ta-run-code-outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("out")).unwrap();
        let out = tool_with_tools(CodeConfig::default(), r#"["read_file","write_file"]"#)
            .run_with_emit(
                &json!({"code": r#"
                    try { tools.call("write_file", {path: "out/owned.txt", content: "x"}); return {escaped: true}; }
                    catch (e) { return {escaped: false, err: String(e)}; }"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        let ok = serde_json::from_str::<Value>(&out.content).unwrap_or(json!(null));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(ok["escaped"], json!(false), "{}", out.content);
        assert!(
            ok["err"].as_str().unwrap_or("").contains("symlink"),
            "rejection must name the symlink refusal: {}",
            out.content
        );
        assert!(
            !outside.join("owned.txt").exists(),
            "nothing may land outside"
        );
    }

    /// 2026-09 external re-review P1: the symlink walk used to fold `..`
    /// FIRST, which erases the symlink from the path it inspects —
    /// `link/../x` handed the policy layer a clean lexical form ("x",
    /// inside the workdir) while the kernel resolves the actual WRITE
    /// through `link` first and the file lands OUTSIDE. The walk must
    /// follow the ORIGINAL spelling: push-and-check each Normal component,
    /// pop on ParentDir. Also pins the no-false-positive side (`..` past a
    /// real directory stays writable) and the copy_bytes dst arm (same
    /// shared check).
    #[cfg(unix)]
    #[test]
    fn write_via_symlink_dotdot_escape_rejected() {
        let outer = std::env::temp_dir().join(format!("ta-run-code-esc-{}", std::process::id()));
        std::fs::create_dir_all(outer.join("sub")).unwrap();
        let dir = outer.join("inside");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::os::unix::fs::symlink(outer.join("sub"), dir.join("link")).unwrap();
        std::fs::write(dir.join("src.bin"), "payload").unwrap();
        let out = tool_with_tools(CodeConfig::default(), r#"["read_file","write_file"]"#)
            .run_with_emit(
                &json!({"code": r#"
                    let esc, cp, ok;
                    try { tools.call("write_file", {path: "link/../escaped.txt", content: "pwn"}); esc = "wrote"; }
                    catch (e) { esc = String(e); }
                    try { tools.call("copy_bytes", {src: "src.bin", dst: "link/../escaped.bin"}); cp = "copied"; }
                    catch (e) { cp = String(e); }
                    try { tools.call("write_file", {path: "real/../ok.txt", content: "fine"}); ok = "wrote"; }
                    catch (e) { ok = String(e); }
                    return {esc: esc.slice(0, 80), cp: cp.slice(0, 80), ok: ok};"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        let r = serde_json::from_str::<Value>(&out.content).unwrap_or(json!(null));
        assert!(!out.is_error, "{}", out.content);
        assert!(
            r["esc"].as_str().unwrap_or("").contains("symlink"),
            "the write escape must be refused and name the symlink: {}",
            out.content
        );
        assert!(
            r["cp"].as_str().unwrap_or("").contains("symlink"),
            "the copy_bytes dst escape must be refused too: {}",
            out.content
        );
        assert!(
            !outer.join("escaped.txt").exists() && !outer.join("escaped.bin").exists(),
            "nothing may land outside via link/.."
        );
        // No false positive: `..` past a REAL directory component is plain
        // spelling noise, the original-spelling walk must not reject it.
        assert_eq!(
            r["ok"],
            json!("wrote"),
            "real-dir .. must stay writable: {}",
            out.content
        );
        assert!(
            dir.join("ok.txt").exists(),
            "the benign write must have landed"
        );
    }

    /// 2026-09 external re-review P1 (follow-up): an ABSOLUTE spelling whose
    /// literal front is NOT the canonical workdir but which collapses back
    /// inside — `<outside>/../<workdir>/link/../escaped.txt` — used to skip
    /// the symlink walk entirely (the original-spelling strip_prefix failed)
    /// and was then judged "inside" by workdir_relative's pure-lexical
    /// fallback, while the kernel resolved the WRITE through `link` and
    /// landed it outside. Root-anchored walking + deepest-existing-ancestor
    /// resolution close both halves. Also pins the no-false-positive side
    /// (same collapse shape popping only REAL dirs stays writable) and the
    /// `missing/../file` fallback (interception stays out of the way; the
    /// kernel's ENOENT reports it).
    #[cfg(unix)]
    #[test]
    fn write_via_absolute_spelling_collapse_escape_rejected() {
        let outer = std::env::temp_dir().join(format!("ta-run-code-abs-{}", std::process::id()));
        std::fs::create_dir_all(outer.join("sub")).unwrap();
        let dir = outer.join("inside");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(outer.join("sub"), dir.join("link")).unwrap();
        // Absolute spellings must be built from the CANONICAL paths — the
        // policy layer compares against the canonical workdir, so spellings
        // through an unresolved prefix (macOS /var → /private/var) were
        // always "outside" refusals, before and after this fix.
        let dir_canon = dir.canonicalize().unwrap();
        let outer_canon = outer.canonicalize().unwrap();
        // Absolute, literal front outside the workdir, collapses inside —
        // and traverses the symlink mid-spelling. (`sub/..` pops back to the
        // shared parent, then the spelling continues into the workdir —
        // exactly the report's `outside/../work/link/../escaped.txt` shape.)
        let esc = format!(
            "{}/sub/../inside/link/../escaped.txt",
            outer_canon.display()
        );
        // Same collapse shape, but every popped component is a REAL dir.
        let ok = format!("{}/sub/../inside/real_ok.txt", outer_canon.display());
        // A `..` behind a NOT-YET-EXISTING component: canonicalize fails and
        // the ancestor walk bails (file_name of a `..`-terminated path is
        // None) to the lexical fallback — the interception must NOT turn this
        // into a policy refusal; what happens next is the tool's own business
        // (write_file creates parents, so this one lands inside the workdir).
        let hole = format!("{}/missing/../hole.txt", dir_canon.display());
        let code = format!(
            r#"
            let esc, ok, hole;
            try {{ tools.call("write_file", {{path: {esc:?}, content: "pwn"}}); esc = "wrote"; }}
            catch (e) {{ esc = String(e); }}
            try {{ tools.call("write_file", {{path: {ok:?}, content: "fine"}}); ok = "wrote"; }}
            catch (e) {{ ok = String(e); }}
            try {{ tools.call("write_file", {{path: {hole:?}, content: "x"}}); hole = "wrote"; }}
            catch (e) {{ hole = String(e); }}
            return {{esc: esc.slice(0, 120), ok: ok.slice(0, 120), hole: hole.slice(0, 120)}};"#
        );
        let out = tool_with_tools(CodeConfig::default(), r#"["read_file","write_file"]"#)
            .run_with_emit(&json!({"code": code}), &ctx(&dir), &mut |_| {})
            .unwrap();
        let r = serde_json::from_str::<Value>(&out.content).unwrap_or(json!(null));
        assert!(!out.is_error, "{}", out.content);
        assert!(
            r["esc"].as_str().unwrap_or("").contains("symlink"),
            "the absolute-spelling escape must be refused and name the symlink: {}",
            out.content
        );
        assert!(
            !outer.join("escaped.txt").exists(),
            "nothing may land outside via the collapse spelling"
        );
        assert_eq!(
            r["ok"],
            json!("wrote"),
            "real-dir collapse spellings must stay writable: {}",
            out.content
        );
        assert!(dir.join("real_ok.txt").exists(), "the benign write landed");
        let hole = r["hole"].as_str().unwrap_or("");
        assert!(
            !hole.contains("symlink") && !hole.contains("limited to the working"),
            "missing/../file must NOT be a policy refusal (kernel ENOENT instead): {hole}"
        );
        std::fs::remove_dir_all(&outer).ok();
    }

    /// workdir_relative's nonexistent-target fallback must be KERNEL-TRUTH:
    /// `link/../x` resolves THROUGH the symlink (deepest-existing-ancestor
    /// canonicalize), so the judgment is "outside" — the pure-lexical
    /// fallback it replaced called the same spelling "x" (inside).
    /// `missing/../x` keeps the lexical fallback (the ancestor walk cannot
    /// resolve a not-yet-existing component in front of `..`).
    #[cfg(unix)]
    #[test]
    fn workdir_relative_resolves_missing_target_kernel_truthfully() {
        let outer = std::env::temp_dir().join(format!("ta-run-code-wr-{}", std::process::id()));
        std::fs::create_dir_all(outer.join("sub")).unwrap();
        let dir = outer.join("inside");
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::os::unix::fs::symlink(outer.join("sub"), dir.join("link")).unwrap();
        let canon = dir.canonicalize().unwrap();
        assert_eq!(
            workdir_relative(&canon, "link/../x.txt"),
            None,
            "link/../x resolves outside: must be judged outside"
        );
        assert_eq!(
            workdir_relative(&canon, "real/../x.txt"),
            Some("x.txt".to_string()),
            "real-dir .. still collapses inside"
        );
        assert_eq!(
            workdir_relative(&canon, "missing/../x.txt"),
            Some("x.txt".to_string()),
            "missing-component .. keeps the lexical fallback"
        );
        std::fs::remove_dir_all(&outer).ok();
    }

    /// Sec-review M-2: a ~500k-segment guest path is a payload, not a
    /// filename — it must cost neither host stack (recursive glob matcher)
    /// nor time. Reads fall through and fail at the tool; writes are denied.
    #[test]
    fn absurdly_deep_path_is_not_a_recursion_bomb() {
        let deep = format!("{}x", "a/".repeat(60_000));
        let dir = scratch();
        let tool = tool_with_tools(CodeConfig::default(), r#"["read_file","write_file"]"#);
        let read = tool
            .run_with_emit(
                &json!({"code": r#"
                    try { tools.call("read_file", {path: PATH}); return {read: "ok"}; }
                    catch (e) { return {read: "err"}; }"#.replace("PATH", &format!("{deep:?}"))}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        // Completing at all is the assertion (the old recursive matcher
        // aborted the process); the tool itself fails on the absurd path.
        assert!(!read.is_error, "{}", read.content);
        assert!(
            read.content.contains("\"err\""),
            "no real file exists at that path: {}",
            read.content
        );
        let write = tool
            .run_with_emit(
                &json!({"code": r#"
                    try { tools.call("write_file", {path: PATH, content: "x"}); return {write: "ok"}; }
                    catch (e) { return {write: "err"}; }"#.replace("PATH", &format!("{deep:?}"))}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(
            write.content.contains("\"err\""),
            "pathological writes are denied (fail-closed): {}",
            write.content
        );
    }

    /// §6 中-3: console output is capped at 2× max_result_chars on the HOST
    /// side — a log-flooding loop costs fuel, not host memory — and the drop
    /// count lands in the audit meta.
    #[test]
    fn log_flood_is_capped_and_counted() {
        let cfg = CodeConfig {
            max_result_chars: 200,
            ..CodeConfig::default()
        };
        let mut events: Vec<AgentEvent> = Vec::new();
        let out = tool_with(cfg)
            .run_with_emit(
                &json!({"code": r#"const s = "0123456789"; for (let i = 0; i < 5000; i++) console.log(s); return "done";"#}),
                &ctx(&scratch()),
                &mut |e| events.push(e),
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let meta = out.meta.expect("audit meta");
        assert!(
            meta["logs_dropped"].as_u64().unwrap_or(0) > 4000,
            "the flood past the cap must be counted: {meta}"
        );
        assert!(
            out.content.chars().count() <= 250,
            "collected logs must respect the cap: {}",
            out.content.chars().count()
        );
        // CodeLog events stream per COLLECTED line and stop with collection
        // (§6: no event flood past the cap) — 5000 logged, ≤40 collected.
        let logged = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::CodeLog(_)))
            .count();
        assert!(logged <= 40, "events must stop at the cap: {logged}");
        assert!(logged > 0, "the collected lines did stream: {logged}");
    }

    /// A-3's Deny leg (review: only Allow/Modify were pinned): a hooks Deny
    /// throws inside the guest, `after_tool` fires with executed=false at
    /// decision time, and the call lands in the trace as ok:false.
    #[test]
    fn hooks_deny_throws_and_audits() {
        use crate::hooks::{Hooks, ToolDecision};
        use std::sync::Mutex;

        struct DenyHooks {
            afters: Mutex<Vec<bool>>,
        }
        impl Hooks for DenyHooks {
            fn before_tool(&self, tool: &str, _i: &Value, _c: &ToolCtx) -> ToolDecision {
                ToolDecision::Deny(format!("no {tool} for you"))
            }
            fn after_tool(
                &self,
                _t: &str,
                _i: &Value,
                _o: &ToolOutput,
                executed: bool,
                _ctx: &ToolCtx,
            ) {
                self.afters.lock().unwrap().push(executed);
            }
        }
        let hooks = Arc::new(DenyHooks {
            afters: Mutex::new(Vec::new()),
        });
        let dir = scratch();
        let mut c = ctx(&dir);
        c.hooks = Some(hooks.clone());
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"
                    try { tools.call("read_file", {path: "x.txt"}); return {denied: false}; }
                    catch (e) { return {denied: true, msg: String(e), kind: e.kind, retryable: e.retryable}; }"#}),
                &c,
                &mut |_| {},
            )
            .unwrap();
        let ok = serde_json::from_str::<Value>(&out.content).unwrap_or(json!(null));
        assert_eq!(ok["denied"], json!(true), "{}", out.content);
        assert!(
            ok["msg"]
                .as_str()
                .unwrap_or("")
                .contains("no read_file for you"),
            "the Deny reason is the thrown message: {}",
            out.content
        );
        // v2 review: hooks denials carry their own structured class.
        assert_eq!(ok["kind"], json!("hooks"), "{}", out.content);
        assert_eq!(ok["retryable"], json!(false), "{}", out.content);
        assert_eq!(
            *hooks.afters.lock().unwrap(),
            vec![false],
            "executed=false at decision time"
        );
        let meta = out.meta.expect("audit meta");
        assert_eq!(
            meta["calls"][0]["ok"],
            json!(false),
            "denied call audits as not-ok: {meta}"
        );
    }

    /// A-9 / review: an unknown tool name still passes the gate first (same
    /// order as the top-level loop), then fails at the lookup.
    #[test]
    fn unknown_tool_still_passes_gate_first() {
        use crate::hooks::{Hooks, ToolDecision};
        use std::sync::Mutex;

        struct Seen(Mutex<Vec<String>>);
        impl Hooks for Seen {
            fn before_tool(&self, tool: &str, _i: &Value, _c: &ToolCtx) -> ToolDecision {
                self.0.lock().unwrap().push(tool.into());
                ToolDecision::Allow
            }
        }
        let hooks = Arc::new(Seen(Mutex::new(Vec::new())));
        let dir = scratch();
        let mut c = ctx(&dir);
        c.hooks = Some(hooks.clone());
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"
                    try { tools.call("definitely_not_a_tool", {}); return {found: true}; }
                    catch (e) { return {found: false}; }"#}),
                &c,
                &mut |_| {},
            )
            .unwrap();
        assert!(
            out.content.contains("\"found\":false"),
            "unknown tool throws in the guest: {}",
            out.content
        );
        assert_eq!(
            *hooks.0.lock().unwrap(),
            vec!["definitely_not_a_tool".to_string()],
            "the gate sees even unknown names"
        );
    }

    /// Test 10's 低-1 latch half (arch-review #6): once the call limit halts
    /// the sandbox, even the "free" `tools.list()` is refused — a busy loop
    /// cannot sidestep the audit ceiling.
    #[test]
    fn halted_sandbox_refuses_even_tools_list() {
        let cfg = CodeConfig {
            max_tool_calls: 0,
            ..CodeConfig::default()
        };
        let out = run_code(
            r#"
                let refused = null;
                try { tools.call("read_file", {path: "x"}); } catch (e) { refused = "call"; }
                let list_refused = false;
                try { tools.list(); } catch (e) { list_refused = true; }
                return {refused, list_refused};"#,
            cfg,
        );
        let ok = serde_json::from_str::<Value>(&out.content).unwrap_or(json!(null));
        assert_eq!(ok["refused"], json!("call"), "{}", out.content);
        assert_eq!(ok["list_refused"], json!(true), "{}", out.content);
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

    /// Test 9 (image outputs, milestone gap per arch-review #6): a tool
    /// returning an image reports an explicit error envelope to the guest —
    /// binaries cannot ride the JSON channel. The image-bearing tool is a
    /// host CallbackTool that OPTS INTO the guest registry
    /// (`with_code_access`): `load_image` itself never does (top-level-only
    /// by design), so allow-listing it — as an earlier revision of this test
    /// did — only exercised the "unknown tool: load_image" error whose text
    /// happens to contain "image"; the real branch below ran zero times
    /// (external re-review: false-green test).
    #[test]
    fn image_output_is_an_explicit_error() {
        use crate::tools::CallbackTool;
        let dir = scratch();
        // Canonical 1×1 transparent PNG.
        const PNG: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x62, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let img_tool = CallbackTool::new(
            ToolDef {
                name: "host_image".into(),
                description: "host callback that returns an image".into(),
                schema: json!({"type": "object", "properties": {
                    "path": {"type": "string"}
                }}),
            },
            move |_input: &Value, _ctx: &ToolCtx| {
                use base64::Engine as _;
                Ok(ToolOutput {
                    content: "[image loaded]".into(),
                    image: Some(crate::provider::Block::Image {
                        media_type: "image/png".into(),
                        data: base64::engine::general_purpose::STANDARD.encode(PNG),
                    }),
                    is_error: false,
                    usage: None,
                    usage_in_shared_budget: false,
                    meta: None,
                })
            },
        )
        .with_code_access();
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":["read_file"],"code":{"enabled":true}}"#,
        )
        .unwrap();
        let tool = RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(CodeConfig::default())),
            &reqwest::blocking::Client::new(),
            "k",
            vec![],
            vec![Arc::new(Box::new(img_tool) as Box<dyn Tool>)],
        );
        let out = tool
            .run_with_emit(
                &json!({"code": r#"
                    try { tools.call("host_image", {path: "p.png"}); return {carried: true}; }
                    catch (e) { return {carried: false, msg: String(e)}; }"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        let ok = serde_json::from_str::<Value>(&out.content).unwrap_or(json!(null));
        assert_eq!(ok["carried"], json!(false), "{}", out.content);
        // The REAL branch's message (run_code's transport-level refusal), not
        // "unknown tool" noise that merely contains "image".
        assert!(
            ok["msg"].as_str().unwrap_or("").contains("cannot carry"),
            "the error must say why: {}",
            out.content
        );
    }

    /// Write half of the A-8 boundary (the read half — `cat .env` passing
    /// through — is pinned by A-8 itself): the path-interception layer binds
    /// to the 4 file tools (and copy_bytes) ONLY. `shell` is deliberately
    /// un-intercepted (D13: the layer guards model MISREADS, not hostile
    /// guests), so its writes — redirects included — bypass fs_write_dirs and
    /// the write-side symlink refusal. This test PINS that bypass so the
    /// write side is as explicit as the read side; the hostile-guest remedies
    /// stay restrict_to_workdir / hooks Deny / dropping shell from `tools`.
    #[test]
    fn shell_writes_bypass_the_file_tool_write_policy_a8() {
        let dir = scratch();
        // Non-empty fs_write_dirs → file-tool writes are confined to it.
        let code = CodeConfig {
            fs_write_dirs: vec!["allowed".into()],
            ..CodeConfig::default()
        };
        let out = tool_with_tools(code, r#"["read_file","write_file","shell"]"#)
            .run_with_emit(
                &json!({"code": r#"
                    var refused = false;
                    try { tools.call("write_file", {path: "out.txt", content: "x"}); }
                    catch (e) { refused = true; }
                    tools.call("shell", {command: "echo via-shell > out.txt"});
                    return {refused: refused};"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        let ok = serde_json::from_str::<Value>(&out.content).unwrap_or(json!(null));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            ok["refused"],
            json!(true),
            "write_file outside fs_write_dirs must be refused"
        );
        let wrote = std::fs::read_to_string(dir.join("out.txt"));
        assert_eq!(
            wrote.as_deref().unwrap_or_default().trim(),
            "via-shell",
            "shell's redirect write must NOT be intercepted (A-8 write side)"
        );
    }

    /// Registry correctness (all three reviews): the shared MCP tools join
    /// the guest registry — `tools.list()` names them and `tools.call`
    /// reaches the real server. Hermetic builds can't construct an McpTool,
    /// so this runs the real echo server (same shape as
    /// `hot_add_remove_mcp_server_live`).
    #[test]
    #[ignore = "live: spawns the python3 echo MCP server (examples/mcp_echo.py)"]
    fn guest_reaches_mcp_tools_live() {
        use crate::config::McpServerConfig;
        let echo = McpServerConfig {
            name: "e1".into(),
            command: Some("python3".into()),
            args: vec![format!(
                "{}/examples/mcp_echo.py",
                env!("CARGO_MANIFEST_DIR")
            )],
            url: None,
            transport: None,
            headers: Default::default(),
            env: Default::default(),
        };
        let mcp_tools = mcp::build(&[echo]).expect("echo server starts");
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":["read_file"]}"#,
        )
        .unwrap();
        let tool = RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(CodeConfig {
                enabled: true,
                ..CodeConfig::default()
            })),
            &reqwest::blocking::Client::new(),
            "k",
            mcp_tools,
            vec![],
        );
        let out = tool
            .run_with_emit(
                &json!({"code": r#"
                    const list = tools.list();
                    const listed = list.some(t => t.name === "mcp_e1_echo");
                    let text = null;
                    try { text = tools.call("mcp_e1_echo", {text: "hi"}); } catch (e) { text = String(e); }
                    return {listed, text};"#}),
                &ctx(&scratch()),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("\"listed\":true"),
            "the MCP tool must be in the guest's tools.list(): {}",
            out.content
        );
        assert!(
            out.content.contains("echo: hi"),
            "the guest call must reach the real server: {}",
            out.content
        );
        // Dropping the tool drops the last McpTool clones → the transport
        // kills the child process group; verify no residue (live-test pact).
        drop(tool);
        if let Ok(pg) = std::process::Command::new("pgrep")
            .arg("-f")
            .arg("mcp_echo.py")
            .output()
        {
            assert!(
                pg.stdout.is_empty(),
                "echo server leaked: {}",
                String::from_utf8_lossy(&pg.stdout)
            );
        }
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

    /// sec-review M-1: zero-char log lines must cost against the host log
    /// cap — `console.log()` with no args is 0 chars and a chars-only check
    /// let `while (1) console.log()` collect unboundedly (and emit one
    /// CodeLog event per line) on default fuel.
    #[test]
    fn zero_char_log_flood_is_capped_and_counted() {
        let cfg = CodeConfig {
            max_result_chars: 100, // host cap = 2×100 → ≤200 collected lines
            ..CodeConfig::default()
        };
        let mut events: Vec<AgentEvent> = Vec::new();
        let dir = scratch();
        let out = tool_with(cfg)
            .run_with_emit(
                &json!({"code": r#"for (let i = 0; i < 20000; i++) console.log(); return "done";"#}),
                &ctx(&dir),
                &mut |e| events.push(e),
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let meta = out.meta.expect("audit meta");
        assert!(
            meta["logs_dropped"].as_u64().unwrap_or(0) > 19_000,
            "empty lines past the cap must be counted: {meta}"
        );
        let collected = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::CodeLog(_)))
            .count();
        assert!(
            collected <= 200,
            "collected lines respect the cap: {collected}"
        );
        assert!(collected > 0, "the in-cap lines did stream: {collected}");
    }

    /// §13-5 cancel half of the entry gate (arch-review #2): a cancel flag
    /// already set at entry refuses to admit the guest AT ALL — pure field
    /// path, deterministic (the epoch callback would only observe at the
    /// next tick).
    #[test]
    fn entry_gate_refuses_when_cancel_already_set() {
        use std::sync::atomic::AtomicBool;
        let flag = Arc::new(AtomicBool::new(true));
        let dir = scratch();
        let mut c = ctx(&dir);
        c.cancel = Some(flag);
        let out = tool_with(CodeConfig::default())
            .run_with_emit(&json!({"code": "return 1;"}), &c, &mut |_| {})
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("cancelled") && out.content.contains("before the sandbox started"),
            "{}",
            out.content
        );
        let meta = out.meta.expect("audit meta");
        assert_eq!(meta["call_count"], 0);
        assert_eq!(meta["fuel_used"], 0, "the guest never executed: {meta}");
    }

    /// 2026-09 review #7 — the host_call ENTRY half of the same gate: a cancel
    /// that fires while the guest is EXECUTING (between host calls) used to
    /// let the next `tools.call` start anyway — the epoch trap only lands at
    /// the next ~10ms check point, and a call admitted in that window ran a
    /// real inner tool to completion, past the documented "in-flight calls
    /// run to completion" boundary (which promises calls already running, not
    /// new admissions). Deterministic despite the epoch race: whether the
    /// entry check refuses the second call or the trap beats the guest to it,
    /// exactly ONE call is audited and the tombstone attributes cancel.
    #[test]
    fn host_call_entry_refuses_new_calls_after_cancel() {
        use crate::tools::CallbackTool;
        use std::sync::atomic::AtomicBool;
        let flag = Arc::new(AtomicBool::new(false));
        let trip = {
            let flag = flag.clone();
            CallbackTool::new(
                ToolDef {
                    name: "trip_cancel".into(),
                    description: "flips the host cancel flag".into(),
                    schema: json!({"type": "object"}),
                },
                move |_input: &Value, _ctx: &ToolCtx| {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    Ok(ToolOutput::text(String::from("flag set")))
                },
            )
            .with_code_access()
        };
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":["read_file"],"code":{"enabled":true}}"#,
        )
        .unwrap();
        let tool = RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(CodeConfig::default())),
            &reqwest::blocking::Client::new(),
            "k",
            vec![],
            vec![Arc::new(Box::new(trip) as Box<dyn Tool>)],
        );
        let dir = scratch();
        let mut c = ctx(&dir);
        c.cancel = Some(flag);
        let out = tool
            .run_with_emit(
                &json!({"code": r#"
                    tools.call("trip_cancel", {});
                    tools.call("trip_cancel", {});
                    return "both ran";
                "#}),
                &c,
                &mut |_| {},
            )
            .unwrap();
        let meta = out.meta.expect("audit meta");
        assert_eq!(
            meta["call_count"], 1,
            "second call must not be admitted: {meta}"
        );
        assert!(
            out.content.contains("cancelled (host cancel flag)"),
            "tombstone must attribute the stop to cancel: {}",
            out.content
        );
    }

    /// 2026-09 external re-review P2 — the GATE→EXECUTION half: the §13-5
    /// entry gate runs BEFORE before_tool, so a cancel that lands while the
    /// hooks gate is deliberating (a host approval dialog waiting on a
    /// human) used to let the just-approved inner tool run anyway — the
    /// sandbox "ran cancelled" with real side effects. The checkpoint
    /// re-checks the flag between the gate and the run; the tombstone
    /// vocabulary matches the entry gate's (CANCEL_HALTED_WHY, kind
    /// "cancel"). Deterministic: the hook sets the flag on the same thread
    /// that then evaluates the checkpoint.
    #[test]
    fn gate_cancel_checkpoint_refuses_inner_call_after_approval_wait() {
        use crate::hooks::Hooks;
        use crate::tools::CallbackTool;
        use std::sync::atomic::AtomicBool;

        struct SetCancelThenAllow(Arc<AtomicBool>);
        impl Hooks for SetCancelThenAllow {
            fn before_tool(
                &self,
                _name: &str,
                _input: &Value,
                _ctx: &ToolCtx,
            ) -> crate::hooks::ToolDecision {
                self.0.store(true, std::sync::atomic::Ordering::Relaxed);
                crate::hooks::ToolDecision::Allow
            }
        }

        let flag = Arc::new(AtomicBool::new(false));
        let ran = Arc::new(AtomicBool::new(false));
        let side_effect = {
            let ran = ran.clone();
            CallbackTool::new(
                ToolDef {
                    name: "writer".into(),
                    description: "must never execute under this test".into(),
                    schema: json!({"type": "object"}),
                },
                move |_input: &Value, _ctx: &ToolCtx| {
                    ran.store(true, std::sync::atomic::Ordering::Relaxed);
                    Ok(ToolOutput::text(String::from("ran")))
                },
            )
            .with_code_access()
        };
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":["read_file"],"code":{"enabled":true}}"#,
        )
        .unwrap();
        let tool = RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(CodeConfig::default())),
            &reqwest::blocking::Client::new(),
            "k",
            vec![],
            vec![Arc::new(Box::new(side_effect) as Box<dyn Tool>)],
        );
        let dir = scratch();
        let mut c = ctx(&dir);
        c.cancel = Some(flag.clone());
        c.hooks = Some(Arc::new(SetCancelThenAllow(flag)));
        let out = tool
            .run_with_emit(
                &json!({"code": r#"tools.call("writer", {}); return "unreachable";"#}),
                &c,
                &mut |_| {},
            )
            .unwrap();
        assert!(
            !ran.load(std::sync::atomic::Ordering::Relaxed),
            "the just-approved call must not run after cancel"
        );
        assert!(
            out.content.contains("cancelled (host cancel flag)"),
            "tombstone must match the entry-gate vocabulary: {}",
            out.content
        );
        let meta = out.meta.expect("audit meta");
        assert_eq!(
            meta["call_count"], 1,
            "the refused call is still audited: {meta}"
        );
    }

    /// 2026-09 external re-review P1 — the CAUGHT-exception half of the entry
    /// gate: the entry rejection is a JS-catchable error, and the old gate
    /// re-checked the host flag only while `interrupt` was still None — a
    /// guest that caught the rejection and retried re-entered with
    /// `interrupt` latched, the cancel arm SKIPPED, and the retried call ran
    /// a real inner tool until the ~10ms epoch trap. The fix latches
    /// `halted` alongside `interrupt` (same shape as the deadline arm), so
    /// the halt check refuses every post-catch retry. Deterministic despite
    /// the epoch race: whether the halt check refuses each retried call or
    /// the epoch trap beats the guest to it, only the ONE pre-cancel call is
    /// ever admitted — the loop counts refusals so a fast guest that finishes
    /// before the first tick is accepted too.
    #[test]
    fn caught_cancel_exception_does_not_admit_retried_calls() {
        use crate::tools::CallbackTool;
        use std::sync::atomic::AtomicBool;
        let flag = Arc::new(AtomicBool::new(false));
        let trip = {
            let flag = flag.clone();
            CallbackTool::new(
                ToolDef {
                    name: "trip_cancel".into(),
                    description: "flips the host cancel flag".into(),
                    schema: json!({"type": "object"}),
                },
                move |_input: &Value, _ctx: &ToolCtx| {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    Ok(ToolOutput::text(String::from("flag set")))
                },
            )
            .with_code_access()
        };
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":["read_file"],"code":{"enabled":true}}"#,
        )
        .unwrap();
        let tool = RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(CodeConfig::default())),
            &reqwest::blocking::Client::new(),
            "k",
            vec![],
            vec![Arc::new(Box::new(trip) as Box<dyn Tool>)],
        );
        let dir = scratch();
        let mut c = ctx(&dir);
        c.cancel = Some(flag);
        let out = tool
            .run_with_emit(
                &json!({"code": r#"
                    tools.call("trip_cancel", {});
                    var refused = 0;
                    for (var i = 0; i < 50; i++) {
                        try { tools.call("trip_cancel", {}); }
                        catch (e) { refused++; }
                    }
                    return "refused=" + refused;
                "#}),
                &c,
                &mut |_| {},
            )
            .unwrap();
        let meta = out.meta.expect("audit meta");
        assert_eq!(
            meta["call_count"], 1,
            "no retried call may be admitted after the caught cancel: {meta}"
        );
        if out.is_error {
            // The epoch trap caught the loop mid-flight — the tombstone must
            // attribute it to cancel, not to a guest error.
            assert!(
                out.content.contains("cancelled (host cancel flag)"),
                "trap must attribute to cancel: {}",
                out.content
            );
        } else {
            // The guest finished the whole loop before the first epoch tick —
            // every one of the 50 retries must have been refused.
            assert!(
                out.content.contains("refused=50"),
                "guest completed: every retry must be refused: {}",
                out.content
            );
        }
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

    /// FR-10 honest-cost fold: inner tool usage bills into run_code's own
    /// usage via the shared saturating merge (compress::merge_usage).
    /// Unit-level because the hermetic registry has no usage-producing tool
    /// (MCP echo reports none) — the integration gap is recorded in the
    /// design doc backlog.
    #[test]
    fn inner_usage_folds_saturating() {
        let mut acc = Usage {
            input_tokens: u64::MAX / 2,
            output_tokens: 1,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: u64::MAX,
        };
        let inner = Usage {
            input_tokens: u64::MAX,
            output_tokens: u64::MAX,
            cache_creation_input_tokens: 7,
            cache_read_input_tokens: 1,
        };
        if let Some(merged) =
            crate::compress::merge_usage(Some(std::mem::take(&mut acc)), Some(inner))
        {
            acc = merged;
        }
        assert_eq!(acc.input_tokens, u64::MAX, "saturates, never wraps");
        assert_eq!(acc.output_tokens, u64::MAX);
        assert_eq!(acc.cache_creation_input_tokens, 7);
        assert_eq!(acc.cache_read_input_tokens, u64::MAX);
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

    /// Test 17 (A-11): a guest throw surfaces the exception message + stack
    /// + collected logs + the completed-call count.
    #[test]
    fn guest_throw_carries_stack_logs_and_counts() {
        let dir = scratch();
        std::fs::write(dir.join("t.txt"), "x\n").unwrap();
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"
                    console.log("before the throw");
                    tools.call("read_file", {path: "t.txt"});
                    throw new Error("boom");"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("boom"), "{}", out.content);
        assert!(
            out.content.contains("at "),
            "the stack must ride along: {}",
            out.content
        );
        assert!(
            out.content.contains("# console") && out.content.contains("before the throw"),
            "{}",
            out.content
        );
        let meta = out.meta.expect("audit meta");
        assert_eq!(
            meta["call_count"], 1,
            "completed calls still audited: {meta}"
        );
        assert_eq!(meta["calls"][0]["ok"], json!(true), "{meta}");
    }

    /// Test 11 / D3: when value+logs overflow the cap, the LOGS are cut
    /// first — the return value rides whole.
    #[test]
    fn truncation_cuts_logs_before_the_return_value() {
        let cfg = CodeConfig {
            max_result_chars: 100,
            ..CodeConfig::default()
        };
        // Value 47 chars; budget = 100 - 47 - 12(sep) = 41 → the first
        // 30-char line fits whole (31 with its '\n'), the second does not.
        // The old version of this test passed by ONE char of alignment luck
        // while the implementation cut mid-line (correctness-review M-1) —
        // the vectors below pin whole-line cutting explicitly.
        let out = run_code(
            r#"
                console.log("012345678901234567890123456789");
                console.log("second-line-should-be-cut-away");
                return "RETVAL." + "x".repeat(40);"#,
            cfg.clone(),
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("RETVAL.") && out.content.contains(&"x".repeat(40)),
            "the return value rides whole: {}",
            out.content
        );
        assert!(
            out.content.contains("012345678901234567890123456789"),
            "the first log line fits the remaining budget WHOLE: {}",
            out.content
        );
        assert!(
            !out.content.contains("second-line"),
            "overflowing log lines are cut, not the value: {}",
            out.content
        );
        // Whole-line discipline: the assembled content must not end inside
        // a log line (a mid-line cut leaves a half-word right before the
        // truncation boundary).
        assert!(
            !out.content.contains("second-lin\n"),
            "lines are cut whole, never mid-line: {:?}",
            out.content
        );
        assert_eq!(
            out.meta.expect("audit meta")["truncated"],
            json!(true),
            "the cut is reported"
        );

        // Counter-vector from the review: value 40 chars → 40+12+31 = 82,
        // second line (25 with its '\n') would end at 107 — over. Whole-line
        // discipline drops it ENTIRELY; the old mid-line behavior kept a
        // 17-char fragment of it.
        let out = run_code(
            r#"
                console.log("012345678901234567890123456789");
                console.log("second-line-is-dropped-whole");
                return "R." + "x".repeat(38);"#,
            cfg.clone(),
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("012345678901234567890123456789"),
            "first line still fits: {}",
            out.content
        );
        assert!(
            !out.content.contains("second-line"),
            "a line that does not fit WHOLE drops entirely (no fragments): {}",
            out.content
        );
        assert_eq!(out.meta.as_ref().unwrap()["truncated"], json!(true));

        // Exact-fit boundary — value 33 (31 chars + the 2 quotes JSON
        // serialization adds) + 12 sep + 55 logs = 100: both whole lines
        // fit at exactly the cap, nothing is cut.
        let out = run_code(
            r#"
                console.log("012345678901234567890123456789");
                console.log("second-line-exactly-fits");
                return "R." + "x".repeat(29);"#,
            cfg.clone(),
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("second-line-exactly-fits"),
            "both whole lines fit the exact budget: {}",
            out.content
        );
        // `truncated` is SPARSE in the audit meta (only emitted when true).
        assert!(out.meta.unwrap().get("truncated").is_none());

        // Value overflows ALONE: value wins, logs drop entirely (and the
        // value itself is tail-truncated with the standard marker).
        let out = run_code(
            r#"
                console.log("log-line-drops-entirely");
                return "V." + "x".repeat(120);"#,
            cfg,
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(
            !out.content.contains("# console"),
            "no room for any log once the value alone busts the cap: {}",
            out.content
        );
        assert!(out.content.contains("truncated"), "{}", out.content);
        assert_eq!(out.meta.unwrap()["truncated"], json!(true));
    }

    /// A-10b / 16b: a config write through the SHARED Arc (what
    /// `Agent::set_code_config` does) reaches the boxed instance — the NEXT
    /// execution runs under the tightened limit. In-flight executions keep
    /// their entry snapshot by construction (read-once-per-execution).
    #[test]
    fn set_code_config_tightens_the_next_execution() {
        let dir = scratch();
        std::fs::write(dir.join("f.txt"), "x\n").unwrap();
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":["read_file"]}"#,
        )
        .unwrap();
        let shared = Arc::new(RwLock::new(CodeConfig::default()));
        let tool = RunCodeTool::new(
            &cfg,
            shared.clone(),
            &reqwest::blocking::Client::new(),
            "k",
            vec![],
            vec![],
        );
        let code = r#"
            let n = 0;
            try { tools.call("read_file", {path: "f.txt"}); n = 1;
                  tools.call("read_file", {path: "f.txt"}); n = 2; }
            catch (e) {}
            return {n};"#;
        let out1 = tool
            .run_with_emit(&json!({"code": code}), &ctx(&dir), &mut |_| {})
            .unwrap();
        assert_eq!(out1.content, "{\"n\":2}", "{}", out1.content);
        assert_eq!(out1.meta.as_ref().unwrap()["call_count"], 2);

        // The Agent face's carrier write (set_code_config does exactly this
        // through the same Arc).
        *shared.write().unwrap() = CodeConfig {
            max_tool_calls: 1,
            ..CodeConfig::default()
        };
        let out2 = tool
            .run_with_emit(&json!({"code": code}), &ctx(&dir), &mut |_| {})
            .unwrap();
        assert_eq!(
            out2.content, "{\"n\":1}",
            "second call refused: {}",
            out2.content
        );
        assert_eq!(out2.meta.as_ref().unwrap()["call_count"], 1);
    }

    // ── v2 P1: prelude, structured guest I/O, wasmtime cache dir ──────────

    #[test]
    fn builtin_skill_md_is_valid_and_named_run_code() {
        // The built-in skill is embedded via include_str! with no compile-time
        // validation — pin its contract here: frontmatter parses under the
        // handwritten 6-field parser, the name matches what the slim
        // description points at, the description is within the 1024-char
        // standard cap, and the body actually carries the deep contracts the
        // description delegates to it.
        let e =
            crate::skills::embedded_entry(RUN_CODE_SKILL_MD).expect("built-in skill md must parse");
        assert_eq!(e.name, "run-code");
        assert!(e.description.chars().count() <= 1024);
        let body = RUN_CODE_SKILL_MD;
        for needle in [
            "format:\"json\"",
            "raw:true",
            "content_b64",
            "readAll",
            "readLines",
            "writeVerified",
            "next_offset",
            "BigInt",
            "1103515245n",
        ] {
            assert!(body.contains(needle), "skill body missing {needle:?}");
        }
    }

    #[test]
    fn slim_description_points_at_the_skill_and_keeps_helper_list_closed() {
        // The description is the PERMANENT schema surface: it must name the
        // closed helper list (anti-hallucination — the skill body is only
        // loaded on demand) and point at the run-code skill; and it must stay
        // slim (that's the whole point of P2) — bound it so a regression to
        // the old full-contract description fails here.
        let cc: CodeConfig = serde_json::from_str("{}").unwrap();
        let d = tool_with(cc).def().description;
        assert!(d.contains("readAll") && d.contains("readLines") && d.contains("writeVerified"));
        assert!(d.contains("此外没有别的内置助手"));
        assert!(d.contains("run-code"));
        // 数值精度契约必须常驻 schema（bench 实证：Python LCG 直译 JS 没用
        // BigInt，float64 丢低位答 816 而非 1048——描述零精度字样时模型自我背书）
        assert!(d.contains("BigInt") && d.contains("2^53"));
        assert!(
            d.chars().count() <= 1200,
            "description grew to {} chars — move the detail into run_code_skill.md",
            d.chars().count()
        );
    }

    #[test]
    fn prelude_flattens_to_exactly_one_physical_line() {
        // quickjs reports error line numbers relative to the eval'd source:
        // the flattened prelude shifts user frames by a CONSTANT +1. A
        // newline in the flattened form would break that invariant, and a
        // line not ending in `;`/`{`/`}` would glue statements together.
        // Both gate states must hold the invariant (each gated half flattens
        // under the same rules).
        for (include_libs, call_batch, src) in [
            (false, false, PRELUDE_SRC),
            (true, false, PRELUDE_LIB_SRC),
            (false, true, PRELUDE_BATCH_SRC),
        ] {
            let flat = prelude_line(include_libs, call_batch);
            assert!(!flat.contains('\n'), "flattened prelude must be one line");
            assert!(flat.starts_with("var __TA_B64T"), "unexpected head: {flat}");
            for line in src.lines().map(str::trim) {
                if line.is_empty() || line.starts_with("//") {
                    continue;
                }
                assert!(
                    line.ends_with(';') || line.ends_with('{') || line.ends_with('}'),
                    "prelude line must end in ;/{{/}} for separator-free flattening: {line:?}"
                );
            }
        }
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

    /// readAll/readLines: paged assembly via format:"json" + next_offset,
    /// no line-number prefixes, correct line count across page boundaries.
    #[test]
    fn prelude_read_all_and_read_lines_page_through() {
        let dir = scratch();
        // 30_000 short rows → 2 pages at the default pageLines=20_000.
        let content: String = (1..=30_000).map(|i| format!("row-{i}\n")).collect();
        std::fs::write(dir.join("big.txt"), content).unwrap();
        let out = tool_with_tools(CodeConfig::default(), r#"["read_file","write_file"]"#)
            .run_with_emit(
                &json!({"code": r#"
                    var ls = readLines("big.txt");
                    var text = readAll("big.txt");
                    return {n: ls.length, first: ls[0], last: ls[ls.length - 1],
                            chars: text.length, tail: text.slice(-7)};"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // The file is 30_000 newline-TERMINATED rows (288_894 chars incl. the
        // final newline — readAll is byte-faithful), so readLines' split yields
        // a trailing empty element (n=30001, last="") — the documented
        // readLines semantics (skill md: 末尾换行会产生空尾元素).
        assert_eq!(
            out.content,
            r#"{"n":30001,"first":"row-1","last":"","chars":288894,"tail":"-30000\n"}"#
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// writeVerified: the byte-faithful write path — exact bytes on disk
    /// including a trailing newline, verified by read-back inside the guest.
    #[test]
    fn prelude_write_verified_roundtrips_exact_bytes() {
        let dir = scratch();
        let out = tool_with_tools(CodeConfig::default(), r#"["read_file","write_file"]"#)
            .run_with_emit(
                &json!({"code": r#"
                    var a = writeVerified("a.txt", "tail-no-newline");
                    var b = writeVerified("b.txt", "line1\nline2\n");
                    return [a, b];"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read(dir.join("a.txt")).unwrap(),
            b"tail-no-newline"
        );
        assert_eq!(std::fs::read(dir.join("b.txt")).unwrap(), b"line1\nline2\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A Hooks layer that truncates every write_file body to 3 bytes — the
    /// external-review P2-2 repro shape. writeVerified's read-back used to
    /// prefix-match the shortened file (read the whole file → not truncated →
    /// the expected-prefix slice compares equal) and return
    /// "verified 6 chars" with is_error=false; only a total-length check at
    /// EOF catches a SHORTER-than-written file.
    #[test]
    fn prelude_write_verified_rejects_hook_shortened_write() {
        use crate::hooks::{Hooks, ToolDecision};

        struct TruncateTo3;
        impl Hooks for TruncateTo3 {
            fn before_tool(
                &self,
                tool: &str,
                input: &Value,
                _ctx: &crate::tools::ToolCtx,
            ) -> ToolDecision {
                if tool == "write_file" {
                    if let Some(c) = input.get("content").and_then(|v| v.as_str()) {
                        if c.len() > 3 {
                            let mut v = input.clone();
                            v["content"] = Value::String(c[..3].to_string());
                            return ToolDecision::Modify(v);
                        }
                    }
                }
                ToolDecision::Allow
            }
        }

        let dir = scratch();
        let mut guest_ctx = ctx(&dir);
        guest_ctx.hooks = Some(std::sync::Arc::new(TruncateTo3));
        let out = tool_with_tools(CodeConfig::default(), r#"["read_file","write_file"]"#)
            .run_with_emit(
                &json!({"code": r#"return writeVerified("a.txt", "abcdef");"#}),
                &guest_ctx,
                &mut |_| {},
            )
            .unwrap();
        // The hook DID rewrite the file to the 3-byte prefix; the helper must
        // refuse to call that verified instead of blessing the prefix.
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"abc");
        assert!(
            out.is_error,
            "shortened write must not verify: {}",
            out.content
        );
        assert!(
            out.content.contains("length mismatch"),
            "expected the length check to fire, got: {}",
            out.content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The task4 scenario, end to end in the guest: raw read → content_b64
    /// write-back preserves EVERY byte (binary payloads included) without any
    /// text-mode round-trip.
    #[test]
    fn guest_raw_copy_is_byte_exact() {
        let dir = scratch();
        let mut bytes: Vec<u8> = b"\xef\xbb\xbfheader,".to_vec();
        bytes.extend((0u8..=255).cycle().take(3000));
        bytes.push(b'\n'); // trailing newline must survive
        std::fs::write(dir.join("src.bin"), &bytes).unwrap();
        let out = tool_with_tools(CodeConfig::default(), r#"["read_file","write_file"]"#)
            .run_with_emit(
                &json!({"code": r#"
                    var copied = 0;
                    for (;;) {
                        var r = tools.call("read_file", {path: "src.bin", raw: true, offset: copied});
                        if (r.bytes_read === 0) break;
                        tools.call("write_file", {path: copied === 0 ? "dst.bin" : "dst.bin.part" + copied, content_b64: r.content_b64});
                        if (!r.truncated) break;
                        copied += r.bytes_read;
                    }
                    return copied;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // Guest ctx has the 2M data-scale budget → one window covers it all.
        assert_eq!(out.content, "0");
        assert_eq!(std::fs::read(dir.join("dst.bin")).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── code_memory M1: lib family (include / saveLib) ─────────────────────
    //
    // A-M 编号对应 docs/code_memory_design.md §7 的验收清单（A-M8 属 M3）。

    fn include_cfg() -> CodeConfig {
        CodeConfig {
            include: true,
            ..CodeConfig::default()
        }
    }

    /// [`include_cfg`] + write access (saveLib writes the lib file AND the
    /// INDEX upsert through the ordinary file tools).
    fn lib_tool() -> RunCodeTool {
        tool_with_tools(include_cfg(), r#"["read_file","write_file","list_dir"]"#)
    }

    /// Hand-write a lib + a matching st=ok INDEX entry Rust-side (the
    /// "human-reviewed library" leg — no session registration involved).
    fn plant_lib(dir: &std::path::Path, rel: &str, src: &str, st: &str) -> String {
        let lib_rel = format!("lib/{rel}");
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join(&lib_rel), src).unwrap();
        let hex = sha256_hex(src.as_bytes());
        std::fs::write(
            dir.join(LIB_INDEX_REL),
            format!(
                "# lib index\n<!-- ta-lib: path={rel} sha256={hex} needs=- ver=1 st={st} -->\n"
            ),
        )
        .unwrap();
        hex
    }

    fn js_out(tool: &RunCodeTool, dir: &std::path::Path, code: &str) -> Value {
        let out = tool
            .run_with_emit(&json!({ "code": code }), &ctx(dir), &mut |_| {})
            .unwrap();
        assert!(!out.is_error, "guest failed: {}", out.content);
        serde_json::from_str(&out.content).unwrap_or(json!(null))
    }

    /// A-M1: saveLib → include in the same execution; idempotent double
    /// include (one load); `__hash_file` direct call returns a full digest;
    /// audit traces both reserved names.
    #[test]
    fn lib_save_and_include_same_execution() {
        let dir = scratch();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            saveLib("lib/c.js", "globalThis.loads = (globalThis.loads || 0) + 1; function add(a, b) { return a + b; }");
            include("lib/c.js");
            include("lib/c.js");
            var h = tools.call("__hash_file", {path: "lib/c.js"});
            return {sum: add(2, 3), loads: globalThis.loads, hashlen: h.sha256.length};"#,
        );
        assert_eq!(v["sum"], json!(5), "included lib is callable: {v}");
        assert_eq!(v["loads"], json!(1), "double include loads once: {v}");
        assert_eq!(v["hashlen"], json!(64), "full sha256 hex: {v}");
        // The manifest on disk carries the machine line with st=draft (MD-10).
        let index = std::fs::read_to_string(dir.join(LIB_INDEX_REL)).unwrap();
        assert!(index.contains("path=c.js"), "index: {index}");
        assert!(
            index.contains("st=draft"),
            "saveLib never ok's itself: {index}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M1/A-M7 + MD-14 clone-sharing: a draft lib is admitted in a LATER
    /// execution of the SAME tool lineage (session registration survives the
    /// execution), and refused for a FRESH lineage (session is per-lineage,
    /// never persisted).
    #[test]
    fn lib_draft_admission_is_session_scoped() {
        let dir = scratch();
        let tool = lib_tool();
        let out = tool
            .run_with_emit(
                &json!({"code": r#"saveLib("lib/c.js", "function add(a, b) { return a + b; }"); return 1;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // Same lineage, new execution: admitted via session registration.
        let v = js_out(&tool, &dir, r#"include("lib/c.js"); return add(9, 1);"#);
        assert_eq!(v, json!(10), "session-registered draft admits: {v}");
        // Fresh lineage, same disk state: draft without session → refused.
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            try { include("lib/c.js"); return {refused: false}; }
            catch (e) { return {refused: true, kind: e.kind, msg: String(e)}; }"#,
        );
        assert_eq!(v["refused"], json!(true), "{v}");
        assert_eq!(v["kind"], json!("lib"), "{v}");
        assert!(
            v["msg"].as_str().unwrap_or("").contains("st=draft"),
            "error names the admission state: {v}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M7 st=ok leg: a hand-reviewed INDEX (st=ok, matching hash) admits
    /// with NO session registration — the cross-session path.
    #[test]
    fn lib_st_ok_admits_without_session() {
        let dir = scratch();
        plant_lib(&dir, "t.js", "function twice(x) { return x * 2; }", "ok");
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"include("lib/t.js"); return twice(21);"#,
        );
        assert_eq!(v, json!(42), "{v}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M6: content changed after registration → hash mismatch is a hard
    /// refusal even though the session registered the OLD bytes.
    #[test]
    fn lib_tamper_after_registration_rejected() {
        let dir = scratch();
        let tool = lib_tool();
        let out = tool
            .run_with_emit(
                &json!({"code": r#"saveLib("lib/c.js", "function f() { return 1; }"); return 1;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        std::fs::write(dir.join("lib/c.js"), "function f() { return 2; }").unwrap();
        let v = js_out(
            &tool,
            &dir,
            r#"
            try { include("lib/c.js"); return {refused: false}; }
            catch (e) { return {refused: true, kind: e.kind, msg: String(e)}; }"#,
        );
        assert_eq!(v["refused"], json!(true), "{v}");
        assert_eq!(v["kind"], json!("lib"), "{v}");
        assert!(
            v["msg"].as_str().unwrap_or("").contains("does not match"),
            "integrity error: {v}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M10: an unregistered file (no INDEX entry at all) is refused with
    /// guidance toward saveLib.
    #[test]
    fn lib_missing_entry_rejected() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/x.js"), "var x = 1;").unwrap();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            try { include("lib/x.js"); return {refused: false}; }
            catch (e) { return {refused: true, msg: String(e)}; }"#,
        );
        assert_eq!(v["refused"], json!(true), "{v}");
        assert!(
            v["msg"]
                .as_str()
                .unwrap_or("")
                .contains("no entry in lib/INDEX.md"),
            "{v}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M11: duplicate/conflicting INDEX entries are a hard error — no
    /// silent pick between the lines.
    #[test]
    fn lib_duplicate_index_entries_rejected() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/x.js"), "var x = 1;").unwrap();
        let hex = sha256_hex(b"var x = 1;");
        let other = sha256_hex(b"var x = 2;");
        std::fs::write(
            dir.join(LIB_INDEX_REL),
            format!(
                "<!-- ta-lib: path=x.js sha256={hex} needs=- ver=1 st=ok -->\n\
                 <!-- ta-lib: path=x.js sha256={other} needs=- ver=1 st=ok -->\n"
            ),
        )
        .unwrap();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            try { include("lib/x.js"); return {refused: false}; }
            catch (e) { return {refused: true, msg: String(e)}; }"#,
        );
        assert_eq!(v["refused"], json!(true), "{v}");
        assert!(v["msg"].as_str().unwrap_or("").contains("duplicate"), "{v}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M12: the host-side source-read bound — an oversized lib fails with
    /// the cap named, before any admission judgment.
    #[test]
    fn lib_read_cap_error() {
        let dir = scratch();
        let big = "x".repeat(MAX_CODE_BYTES + 1);
        plant_lib(&dir, "big.js", &big, "ok");
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            try { include("lib/big.js"); return {refused: false}; }
            catch (e) { return {refused: true, kind: e.kind, msg: String(e)}; }"#,
        );
        assert_eq!(v["refused"], json!(true), "{v}");
        assert_eq!(v["kind"], json!("lib"), "{v}");
        assert!(v["msg"].as_str().unwrap_or("").contains("read cap"), "{v}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M2: cycle-cut (a → b → a loads each once) and the throwing-eval
    /// marker discipline (marker deleted on throw → a retry re-attempts).
    #[test]
    fn lib_cycle_cut_and_retry_after_throw() {
        let dir = scratch();
        let tool = lib_tool();
        let out = tool
            .run_with_emit(
                &json!({"code": r#"
                    saveLib("lib/a.js", "include('lib/b.js'); globalThis.a = (globalThis.a || 0) + 1;");
                    saveLib("lib/b.js", "include('lib/a.js'); globalThis.b = (globalThis.b || 0) + 1;");
                    return 1;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v = js_out(
            &tool,
            &dir,
            r#"
            include("lib/a.js");
            return {a: globalThis.a, b: globalThis.b};"#,
        );
        assert_eq!(v, json!({"a": 1, "b": 1}), "cycle cut, each lib once: {v}");

        let v = js_out(
            &tool,
            &dir,
            r#"
            saveLib("lib/x.js", "throw new Error('boom');");
            var caught = 0, rethrown = 0;
            try { include("lib/x.js"); } catch (e) { caught = 1; }
            try { include("lib/x.js"); rethrown = 1; } catch (e2) { rethrown = String(e2).indexOf("boom") !== -1 ? 2 : 3; }
            return {caught: caught, second: rethrown};"#,
        );
        assert_eq!(
            v,
            json!({"caught": 1, "second": 2}),
            "throwing lib re-attempts after the marker was deleted: {v}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M3: a hooks Deny on the pseudo-tool ("include" — no dunder) throws
    /// kind="hooks" in the guest, fires after_tool(executed=false), and
    /// audits as not-ok.
    #[test]
    fn lib_hooks_deny_pseudo_tool() {
        use crate::hooks::{Hooks, ToolDecision};
        use std::sync::Mutex;

        struct DenyIncludeHooks {
            afters: Mutex<Vec<bool>>,
        }
        impl Hooks for DenyIncludeHooks {
            fn before_tool(&self, tool: &str, _i: &Value, _c: &ToolCtx) -> ToolDecision {
                if tool == "include" {
                    ToolDecision::Deny("no libs today".into())
                } else {
                    ToolDecision::Allow
                }
            }
            fn after_tool(
                &self,
                _t: &str,
                _i: &Value,
                _o: &ToolOutput,
                executed: bool,
                _ctx: &ToolCtx,
            ) {
                self.afters.lock().unwrap().push(executed);
            }
        }
        let hooks = Arc::new(DenyIncludeHooks {
            afters: Mutex::new(Vec::new()),
        });
        let dir = scratch();
        plant_lib(&dir, "t.js", "var t = 1;", "ok");
        let mut c = ctx(&dir);
        c.hooks = Some(hooks.clone());
        let out = lib_tool()
            .run_with_emit(
                &json!({"code": r#"
                    try { include("lib/t.js"); return {denied: false}; }
                    catch (e) { return {denied: true, kind: e.kind, msg: String(e)}; }"#}),
                &c,
                &mut |_| {},
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["denied"], json!(true), "{}", out.content);
        assert_eq!(v["kind"], json!("hooks"), "{}", out.content);
        assert!(
            v["msg"].as_str().unwrap_or("").contains("no libs today"),
            "{}",
            out.content
        );
        assert_eq!(*hooks.afters.lock().unwrap(), vec![false]);
        let meta = out.meta.expect("audit meta");
        assert_eq!(meta["calls"][0]["name"], json!("__include"));
        assert_eq!(meta["calls"][0]["ok"], json!(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M5: deny_paths covers the lib channel — `lib/**` refuses an
    /// otherwise-valid, st=ok library.
    #[test]
    fn lib_deny_paths_blocks_include() {
        let dir = scratch();
        plant_lib(&dir, "t.js", "var t = 1;", "ok");
        let cfg = CodeConfig {
            deny_paths: vec!["lib/**".into()],
            ..include_cfg()
        };
        let out = tool_with_tools(cfg, r#"["read_file","write_file","list_dir"]"#)
            .run_with_emit(
                &json!({"code": r#"
                    try { include("lib/t.js"); return {denied: false}; }
                    catch (e) { return {denied: true, kind: e.kind, msg: String(e)}; }"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["denied"], json!(true), "{}", out.content);
        assert_eq!(v["kind"], json!("denied"), "{}", out.content);
        assert!(
            v["msg"].as_str().unwrap_or("").contains("deny_paths"),
            "{}",
            out.content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M9 escape legs on the lib channel: `../` spelling escapes and
    /// symlinks under lib/ must be refused by the confinement walk (the same
    /// three-discipline walk the file tools use), for BOTH `__include` and
    /// `__hash_file`. The symlink leg forges a matching-hash INDEX entry —
    /// proving the refusal is the path walk, not a hash/admission miss.
    #[cfg(unix)]
    #[test]
    fn lib_channel_path_escape_legs() {
        use std::os::unix::fs::symlink;
        let dir = scratch();
        plant_lib(&dir, "real.js", "globalThis.fromReal = 1;", "ok");
        // A file OUTSIDE lib/ that the escapes try to reach; give it a valid
        // machine line so only the path walk stands in the way.
        std::fs::write(dir.join("evil.js"), "globalThis.escaped = 1;").unwrap();
        symlink("../evil.js", dir.join("lib/alias.js")).unwrap();
        let hex = sha256_hex(b"globalThis.escaped = 1;");
        let index = std::fs::read_to_string(dir.join(LIB_INDEX_REL)).unwrap();
        std::fs::write(
            dir.join(LIB_INDEX_REL),
            format!("{index}<!-- ta-lib: path=alias.js sha256={hex} needs=- ver=1 st=ok -->\n"),
        )
        .unwrap();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            function attempt(kind, path) {
                try { tools.call(kind, {path: path}); return "ran"; }
                catch (e) { return e.kind + ": " + String(e); }
            }
            return {
                dotsInclude: attempt("__include", "lib/../evil.js"),
                dotsHash: attempt("__hash_file", "lib/../evil.js"),
                linkInclude: attempt("__include", "lib/alias.js"),
                linkHash: attempt("__hash_file", "lib/alias.js"),
                clean: (include("lib/real.js"), globalThis.fromReal === 1),
            };"#,
        );
        for leg in ["dotsInclude", "dotsHash", "linkInclude", "linkHash"] {
            let msg = v[leg].as_str().unwrap_or_default();
            assert!(
                msg.starts_with("denied: ") && msg.contains("under lib/"),
                "{leg} must refuse at the confinement walk: {v}"
            );
        }
        assert_eq!(v["clean"], json!(true), "the honest path still works: {v}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whitespace lib paths (impl-review #3): INDEX machine lines are
    /// whitespace-tokenized on the host — `saveLib("lib/a b.js")` used to
    /// sail through and poison EVERY later include with a malformed-index
    /// error. Rejected at the JS boundary before any write, and the host
    /// channel (`lib_rel`) refuses such paths even when a file exists.
    /// Plus the `__hash_file` read-cap leg: a >1 MiB lib file refuses with
    /// the cap error instead of an unbounded host slurp (impl-review #4).
    #[test]
    fn lib_whitespace_paths_refused_and_hash_read_capped() {
        let dir = scratch();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            var saveMsg = null;
            try { saveLib("lib/a b.js", "var x = 1;"); } catch (e) { saveMsg = String(e); }
            var hashKind = null;
            try { tools.call("__hash_file", {path: "lib/a b.js"}); }
            catch (e) { hashKind = e.kind; }
            return {saveMsg: saveMsg, hashKind: hashKind};"#,
        );
        assert!(
            v["saveMsg"]
                .as_str()
                .unwrap_or_default()
                .contains("whitespace"),
            "JS boundary rejects before any write: {v}"
        );
        assert_eq!(v["hashKind"], json!("denied"), "host channel refuses: {v}");
        assert!(
            !dir.join(LIB_INDEX_REL).exists(),
            "no poisoned INDEX was written"
        );
        // Sparse file just over the cap — the read is bounded, the refusal
        // is the cap error (not an unbounded slurp into host memory).
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        let big = std::fs::File::create(dir.join("lib/big.js")).unwrap();
        big.set_len(MAX_CODE_BYTES as u64 + 4096).unwrap();
        drop(big);
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            try { tools.call("__hash_file", {path: "lib/big.js"}); return {capped: false}; }
            catch (e) { return {capped: true, kind: e.kind, msg: String(e)}; }"#,
        );
        assert_eq!(v["capped"], json!(true), "{}", v);
        assert_eq!(v["kind"], json!("lib"), "{}", v);
        assert!(
            v["msg"]
                .as_str()
                .unwrap_or_default()
                .contains("lib read cap"),
            "{}",
            v
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M2 negative legs: the include channel is an INDIRECT eval in global
    /// scope — `function`/`var` declarations leak to globalThis (the export
    /// contract), while lexical declarations (`let`/`const`/`class`) stay in
    /// the eval's own environment, and a `'use strict'` prologue confines
    /// even `function`/`var`. A QuickJS change to eval scoping must trip
    /// this pin, not a downstream task.
    #[test]
    fn lib_lexical_and_strict_declarations_not_exported() {
        let dir = scratch();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            saveLib("lib/lex.js",
                "function fn() { return 1; }\nvar vv = 2;\nlet ll = 3;\nconst cc = 4;\nclass KK {}\nglobalThis.ran = true;");
            saveLib("lib/strict.js", "'use strict';\nfunction sf() { return 9; }\nvar sv = 9;");
            include("lib/lex.js");
            include("lib/strict.js");
            return {
                fn: typeof fn, vv: typeof vv,
                ll: typeof ll, cc: typeof cc, kk: typeof KK,
                ran: globalThis.ran === true,
                sf: typeof sf, sv: typeof sv,
            };"#,
        );
        assert_eq!(v["fn"], json!("function"), "functions export: {v}");
        assert_eq!(v["vv"], json!("number"), "vars export: {v}");
        for leg in ["ll", "cc", "kk"] {
            assert_eq!(
                v[leg],
                json!("undefined"),
                "lexical declaration '{leg}' must not export: {v}"
            );
        }
        assert_eq!(v["ran"], json!(true), "side effects of the load run: {v}");
        for leg in ["sf", "sv"] {
            assert_eq!(
                v[leg],
                json!("undefined"),
                "strict-mode lib must not export '{leg}': {v}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M10's last leg — the recorded LIMITATION, pinned so it stays a
    /// documented fact rather than a regression: the include gate does not
    /// cover every execution path. A guest can always `readAll` a lib file
    /// (reads are not lib-gated) and `eval` it itself — the lib channel's
    /// admission is load-channel hygiene, not a containment boundary.
    #[test]
    fn lib_readall_eval_still_works() {
        let dir = scratch();
        // st=draft + no session registration → include must refuse…
        plant_lib(&dir, "x.js", "globalThis.viaEval = 41;", "draft");
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            var refused = null;
            try { include("lib/x.js"); refused = false; }
            catch (e) { refused = e.kind; }
            // …but readAll + eval is ordinary guest code and still runs.
            eval(readAll("lib/x.js"));
            return {refused: refused, viaEval: (globalThis.viaEval || 0) + 1};"#,
        );
        assert_eq!(v["refused"], json!("lib"), "draft admission refuses: {v}");
        assert_eq!(
            v["viaEval"],
            json!(42),
            "readAll+eval bypasses admission: {v}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M11: multi-lib upsert never crosses INDEX entries, and a failed
    /// INDEX write (here: a directory squatting on the INDEX path) errors
    /// the whole saveLib WITHOUT leaving a half-registration that admits —
    /// the lib file may exist on disk, but include still refuses.
    #[test]
    fn lib_index_upsert_and_partial_failure() {
        let dir = scratch();
        js_out(
            &lib_tool(),
            &dir,
            r#"
            saveLib("lib/a.js", "globalThis.a = 1;");
            saveLib("lib/b.js", "globalThis.b = 2;");
            saveLib("lib/a.js", "globalThis.a = 10;");
            return 1;"#,
        );
        let index = std::fs::read_to_string(dir.join(LIB_INDEX_REL)).unwrap();
        assert_eq!(
            index.matches("ta-lib:").count(),
            2,
            "upsert replaces, never duplicates: {index}"
        );
        assert!(
            index.contains("path=a.js") && index.contains("path=b.js"),
            "both entries intact: {index}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("lib/a.js")).unwrap(),
            "globalThis.a = 10;",
            "upsert wrote the new content"
        );
        // Partial-failure leg: the INDEX path is unusable → saveLib errors…
        let dir2 = scratch();
        std::fs::create_dir_all(dir2.join("lib/INDEX.md")).unwrap();
        let out = lib_tool()
            .run_with_emit(
                &json!({"code": r#"saveLib("lib/c.js", "var c = 1;"); return 1;"#}),
                &ctx(&dir2),
                &mut |_| {},
            )
            .unwrap();
        assert!(
            out.is_error,
            "INDEX write failure surfaces: {}",
            out.content
        );
        assert!(
            dir2.join("lib/c.js").exists(),
            "the lib file half of the write did land — this is the partial state"
        );
        // …and the half-state does not admit: no entry, refused.
        let v = js_out(
            &lib_tool(),
            &dir2,
            r#"
            try { include("lib/c.js"); return {admitted: true}; }
            catch (e) { return {admitted: false, kind: e.kind}; }"#,
        );
        assert_eq!(v["admitted"], json!(false), "{}", json!(v));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// A-M4 (folded into the audit assertions above) + A-M9: gate OFF is the
    /// old binary's model-visible contract — no prelude helpers, reserved-name
    /// direct calls refused naming the knob, no 【库】 in the description.
    #[test]
    fn gate_off_matches_old_contract() {
        let out = run_code(
            r#"return [typeof include, typeof saveLib];"#,
            CodeConfig::default(),
        );
        assert_eq!(out.content, "[\"undefined\",\"undefined\"]");
        let out = run_code(
            r#"
            try { tools.call("__include", {path: "lib/x.js"}); return {refused: false}; }
            catch (e) { return {refused: true, kind: e.kind, msg: String(e)}; }"#,
            CodeConfig::default(),
        );
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["refused"], json!(true), "{}", out.content);
        assert_eq!(v["kind"], json!("denied"), "{}", out.content);
        assert!(
            v["msg"]
                .as_str()
                .unwrap_or("")
                .contains("code.include=false"),
            "refusal names the knob: {}",
            out.content
        );
        // Both lib-family reserved names refuse the same way when gated off
        // (MD-2: hash is a gated host action, not the introspection class).
        let out = run_code(
            r#"
            try { tools.call("__hash_file", {path: "lib/x.js"}); return {refused: false}; }
            catch (e) { return {refused: true, kind: e.kind, msg: String(e)}; }"#,
            CodeConfig::default(),
        );
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["refused"], json!(true), "{}", out.content);
        assert_eq!(v["kind"], json!("denied"), "{}", out.content);
        assert!(
            v["msg"]
                .as_str()
                .unwrap_or("")
                .contains("code.include=false"),
            "hash refusal names the same knob: {}",
            out.content
        );
        let d = tool_with(CodeConfig::default()).def().description;
        assert!(
            !d.contains("【库】") && !d.contains("saveLib"),
            "gate-off description must stay the old contract"
        );
    }

    /// MD-13 gate-ON description: the 【库】 overlay names exactly the two
    /// new helpers and stays slim (bounded like the base).
    #[test]
    fn gate_on_description_lists_lib_helpers() {
        let d = tool_with(include_cfg()).def().description;
        assert!(d.contains("【库】"), "{d}");
        assert!(
            d.contains("include(path)") && d.contains("saveLib(path, src)"),
            "{d}"
        );
        // Base contract unchanged underneath the overlay.
        assert!(d.contains("readAll") && d.contains("writeVerified"), "{d}");
        assert!(
            d.chars().count() <= 1500,
            "gate-on description grew to {} chars",
            d.chars().count()
        );
    }

    /// saveLib's own input check: non-lib/ paths throw before any write.
    #[test]
    fn save_lib_requires_lib_prefix() {
        let dir = scratch();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            try { saveLib("out/x.js", "var x = 1;"); return {threw: false}; }
            catch (e) { return {threw: true, msg: String(e)}; }"#,
        );
        assert_eq!(v["threw"], json!(true), "{v}");
        assert!(
            v["msg"]
                .as_str()
                .unwrap_or("")
                .contains("must start with lib/"),
            "{v}"
        );
        assert!(!dir.join("out").exists(), "nothing written on refusal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M6 lifecycle e2e: 起草登记 → 新会话 draft 拒 → 负责人批准相同 hash →
    /// 加载 → 局部覆写（本次生效、文件不变）→ saveLib 写回回 draft（原批准作废）
    /// → 再批准后才复可用；内容未变时重新登记可救活。
    #[test]
    fn lib_lifecycle_e2e() {
        let dir = scratch();
        // 1. 起草登记（会话 A）。
        let tool_a = lib_tool();
        let out = tool_a
            .run_with_emit(
                &json!({"code": r#"saveLib("lib/judge.js", "function judge(x) { return x > 0 ? 'pos' : 'neg'; }"); return 1;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // 2. 新会话（fresh lineage）直接加载 draft → 拒。
        let tool_b = lib_tool();
        let v = js_out(
            &tool_b,
            &dir,
            r#"
            try { include("lib/judge.js"); return {refused: false}; }
            catch (e) { return {refused: true}; }"#,
        );
        assert_eq!(v["refused"], json!(true), "draft needs approval: {v}");
        // 3. 负责人批准相同 hash（st: draft → ok，内容不动）。
        let index = std::fs::read_to_string(dir.join(LIB_INDEX_REL)).unwrap();
        std::fs::write(dir.join(LIB_INDEX_REL), index.replace("st=draft", "st=ok")).unwrap();
        // 4. 加载（无会话登记，纯 st=ok 准入）+ 5. 局部覆写本次生效、文件不变。
        let v = js_out(
            &tool_b,
            &dir,
            r#"
            include("lib/judge.js");
            function judge(x) { return x > 0 ? 'POS' : 'neg'; }
            return {overridden: judge(1), onDiskStillOld: readAll("lib/judge.js").indexOf("POS") === -1};"#,
        );
        assert_eq!(v["overridden"], json!("POS"), "{v}");
        assert_eq!(v["onDiskStillOld"], json!(true), "{v}");
        // 6. 写回：新哈希登记、回 draft。
        let out = tool_b
            .run_with_emit(
                &json!({"code": r#"
                    include("lib/judge.js");
                    function judge(x) { return x > 0 ? 'POSITIVE' : 'neg'; }
                    saveLib("lib/judge.js", "function judge(x) { return x > 0 ? 'POSITIVE' : 'neg'; }");
                    return 1;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let index = std::fs::read_to_string(dir.join(LIB_INDEX_REL)).unwrap();
        assert!(
            index.contains("st=draft"),
            "write-back resets draft: {index}"
        );
        // 7. 原批准作废：fresh lineage 再次被拒（回 draft 且其会话未登记）。
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            try { include("lib/judge.js"); return {refused: false}; }
            catch (e) { return {refused: true}; }"#,
        );
        assert_eq!(
            v["refused"],
            json!(true),
            "approval died with the old hash: {v}"
        );
        // 8. 内容未变时重新登记可救活（同 hash，本会话登记）。
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            var src = readAll("lib/judge.js");
            saveLib("lib/judge.js", src);
            include("lib/judge.js");
            return judge(2);"#,
        );
        assert_eq!(v, json!("POSITIVE"), "re-register same bytes revives: {v}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// MD-13/M2 + LD-15/M2: the skill's gated sections ride their
    /// assembly-time gates — off strips the marked block (gate-off skill =
    /// pre-gate contract, still frontmatter-valid), on keeps the file
    /// verbatim. Both sections strip independently.
    #[test]
    fn skill_gated_sections_ride_their_gates() {
        let on = run_code_skill_md(true, true);
        assert!(on.contains("库与复用") && on.contains("批量调用"));
        assert_eq!(on, RUN_CODE_SKILL_MD, "both gates on is the file verbatim");
        let off = run_code_skill_md(false, false);
        assert!(
            !off.contains("库与复用") && !off.contains("批量调用（需配置开启"),
            "gates off strip both sections: {off}"
        );
        assert!(
            !off.contains("saveLib")
                && !off.contains("BEGIN LIB SECTION")
                && !off.contains("callBatch")
                && !off.contains("BEGIN BATCH SECTION"),
            "gate-off body teaches neither feature: {off}"
        );
        // The stripped skill must still parse as a valid built-in.
        assert!(
            crate::skills::embedded_entry(&off).is_some(),
            "stripped skill stays valid"
        );
        // And the gate-off body carries the rest of the guide unchanged —
        // including the shared e.kind line in its pre-gate wording (no "lib"
        // class outside the markers: the gate-off contract stays the old
        // binary's).
        assert!(off.contains("read_file 的三种模式") && off.contains("批量操作纪律"));
        assert!(
            off.contains("`tool`=工具自身失败）、`e.tool`"),
            "shared kind line keeps its pre-gate wording"
        );
        // Each gate strips only its own section.
        let lib_only = run_code_skill_md(true, false);
        assert!(lib_only.contains("库与复用") && !lib_only.contains("BATCH SECTION"));
        let batch_only = run_code_skill_md(false, true);
        assert!(!batch_only.contains("库与复用") && batch_only.contains("callBatch"));
    }

    // ── code_llm M2: callBatch（只读并行批量） ────────────────────────────
    //
    // A-L 编号对应 docs/code_llm_design.md §7 的验收清单；A-L4 的真实 llm
    // 吞吐与 e2e 时间区间重叠证明留 M3/V0-B（里程碑表），这里用进程内真实
    // 线程 + host 侧时间戳重叠钉住并发本身。

    fn batch_cfg() -> CodeConfig {
        CodeConfig {
            call_batch: true,
            ..CodeConfig::default()
        }
    }

    fn batch_tool() -> RunCodeTool {
        tool_with_tools(batch_cfg(), r#"["read_file","write_file","list_dir"]"#)
    }

    /// A-L8（call_batch 半边）: gate off = the pre-batch binary's
    /// model-visible contract — no wrapper in the prelude, reserved-name
    /// direct call falls through to the ordinary unknown-tool error (LD-19
    /// 保留名直调钉住), no 【批量】 in the description.
    #[test]
    fn batch_gate_off_matches_old_contract() {
        let out = run_code(r#"return typeof tools.callBatch;"#, CodeConfig::default());
        assert_eq!(out.content, "\"undefined\"");
        let out = run_code(
            r#"
            try { tools.call("__call_batch", {entries: []}); return {refused: false}; }
            catch (e) { return {refused: true, kind: e.kind, msg: String(e)}; }"#,
            CodeConfig::default(),
        );
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["refused"], json!(true), "{}", out.content);
        assert_eq!(v["kind"], json!("unknown_tool"), "{}", out.content);
        assert!(
            v["msg"].as_str().unwrap_or("").contains("__call_batch"),
            "the unknown-tool path names the called name: {}",
            out.content
        );
        let d = tool_with(CodeConfig::default()).def().description;
        assert!(
            !d.contains("【批量】") && !d.contains("callBatch"),
            "gate-off description must stay the old contract: {d}"
        );
    }

    /// A-L4 basics: results in submission order, auto-parsed contents, the
    /// event pair per entry (ToolCall serial at gate time, ToolResult on
    /// completion), K entries = K audit calls — the wrapper itself is
    /// transport and neither announces nor counts.
    #[test]
    fn batch_submission_order_events_and_quota() {
        let dir = scratch();
        for (n, body) in [("a.txt", "AA"), ("b.txt", "BB"), ("c.txt", "CC")] {
            std::fs::write(dir.join(n), body).unwrap();
        }
        let mut events: Vec<AgentEvent> = Vec::new();
        let out = batch_tool()
            .run_with_emit(
                &json!({"code": r#"
                    const rs = tools.callBatch([
                        {name: "read_file", args: {path: "c.txt", format: "json"}},
                        {name: "read_file", args: {path: "a.txt", format: "json"}},
                        {name: "read_file", args: {path: "b.txt", format: "json"}}
                    ]);
                    return rs.map(r => r.content.trim());"#}),
                &ctx(&dir),
                &mut |ev| events.push(ev),
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(
            v,
            json!(["CC", "AA", "BB"]),
            "results in submission order: {v}"
        );
        // Sequential inline entries: all ToolCalls announced during the
        // serial prep, ToolResults on completion — and the wrapper itself
        // never appears in the event stream.
        let names = |key: &str, is_result: bool| -> Vec<String> {
            events
                .iter()
                .filter_map(|e| match (key, e, is_result) {
                    ("call", AgentEvent::ToolCall { name, .. }, _) => Some(name.clone()),
                    ("result", AgentEvent::ToolResult { name, .. }, _) => Some(name.clone()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            names("call", false),
            vec!["read_file", "read_file", "read_file"],
            "ToolCall serial in submission order"
        );
        assert_eq!(
            names("result", true),
            vec!["read_file", "read_file", "read_file"],
            "ToolResult pairs every announced entry"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCall { name, .. } if name.contains("batch"))),
            "the reserved name is transport, not an announced call"
        );
        let meta = out.meta.expect("audit meta");
        assert_eq!(meta["call_count"], 3, "K entries count as K calls: {meta}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-L4: single-entry failures are entries, not aborts — a tool error,
    /// a non-batchable tool, an unknown tool, and a malformed shape each
    /// become their own array slot (kind carries the class, no `halted` on
    /// non-halt classes), and the batch keeps going.
    #[test]
    fn batch_mixed_outcomes_single_entry_errors() {
        let dir = scratch();
        std::fs::write(dir.join("ok.txt"), "hello").unwrap();
        let v = js_out(
            &batch_tool(),
            &dir,
            r#"
            const rs = tools.callBatch([
                {name: "read_file", args: {path: "ok.txt", format: "json"}},
                {name: "read_file", args: {path: "missing.txt", format: "json"}},
                {name: "write_file", args: {path: "x.txt", content: "no"}},
                {name: "no_such_tool", args: {}},
                {name: "read_file", args: 5}
            ]);
            return rs.map(r => r instanceof Error
                ? {err: true, kind: r.kind, halted: r.halted === undefined ? null : r.halted}
                : {err: false});"#,
        );
        let arr = v.as_array().expect("the wrapper returns an array");
        assert_eq!(arr.len(), 5, "every entry gets a slot: {v}");
        assert_eq!(v[0]["err"], json!(false), "success stays a value: {v}");
        assert_eq!(v[1]["kind"], json!("tool"), "tool failure: {v}");
        assert_eq!(v[2]["kind"], json!("batch"), "non-batchable refused: {v}");
        assert_eq!(v[3]["kind"], json!("unknown_tool"), "unknown name: {v}");
        assert_eq!(
            v[4]["kind"],
            json!("batch"),
            "non-object args is the entry shape error: {v}"
        );
        for i in 1..5 {
            assert_eq!(
                v[i]["halted"],
                json!(null),
                "non-halt classes carry no halted: {v}"
            );
        }
        // The refused write never executed.
        assert!(!dir.join("x.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-L4: a hooks Deny on ONE entry → that entry is a kind="hooks" Error
    /// (after_tool(executed=false) fires) and the batch does NOT abort.
    #[test]
    fn batch_hooks_deny_single_entry_does_not_abort() {
        use crate::hooks::{Hooks, ToolDecision};
        use std::sync::Mutex;

        struct DenySecond {
            afters: Mutex<Vec<(String, bool)>>,
        }
        impl Hooks for DenySecond {
            fn before_tool(&self, tool: &str, input: &Value, _c: &ToolCtx) -> ToolDecision {
                if tool == "read_file" && input["path"].as_str() == Some("b.txt") {
                    return ToolDecision::Deny("not b today".into());
                }
                ToolDecision::Allow
            }
            fn after_tool(
                &self,
                t: &str,
                _i: &Value,
                _o: &ToolOutput,
                executed: bool,
                _c: &ToolCtx,
            ) {
                self.afters.lock().unwrap().push((t.to_string(), executed));
            }
        }
        let dir = scratch();
        std::fs::write(dir.join("a.txt"), "A").unwrap();
        std::fs::write(dir.join("b.txt"), "B").unwrap();
        std::fs::write(dir.join("c.txt"), "C").unwrap();
        let hooks = Arc::new(DenySecond {
            afters: Mutex::new(Vec::new()),
        });
        let mut c = ctx(&dir);
        c.hooks = Some(hooks.clone());
        let out = batch_tool()
            .run_with_emit(
                &json!({"code": r#"
                    const rs = tools.callBatch([
                        {name: "read_file", args: {path: "a.txt", format: "json"}},
                        {name: "read_file", args: {path: "b.txt", format: "json"}},
                        {name: "read_file", args: {path: "c.txt", format: "json"}}
                    ]);
                    return rs.map(r => r instanceof Error ? r.kind : r.content.trim());"#}),
                &c,
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(
            v,
            json!(["A", "hooks", "C"]),
            "denied entry is an Error slot, siblings ran: {v}"
        );
        let afters = hooks.afters.lock().unwrap().clone();
        // after_tool order is gate/completion order, not submission order:
        // the Deny notifies at its gate slot (prep), the executed entries at
        // their completions — the flags themselves are the contract.
        assert_eq!(
            afters,
            vec![
                ("read_file".to_string(), false),
                ("read_file".to_string(), true),
                ("read_file".to_string(), true)
            ],
            "after_tool fires per entry with the honest executed flag"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §4 额度 + LD-9: K entries count as K calls; over-budget tail entries
    /// are kind="budget" halted:true (call-limit latches), and the latch
    /// holds for post-batch single calls.
    #[test]
    fn batch_call_limit_tail_entries_budget_halted() {
        let dir = scratch();
        for n in ["a", "b", "c", "d", "e"] {
            std::fs::write(dir.join(format!("{n}.txt")), n).unwrap();
        }
        let cfg = CodeConfig {
            call_batch: true,
            max_tool_calls: 3,
            ..CodeConfig::default()
        };
        let tool = tool_with_tools(cfg, r#"["read_file","write_file","list_dir"]"#);
        // Same execution throughout: the latch is per-execution by design
        // ("the budget resets every invocation") — the post-batch single
        // call must fail fast within THIS run_code invocation.
        let out = tool
            .run_with_emit(
                &json!({"code": r#"
                    const entries = ["a", "b", "c", "d", "e"].map(n =>
                        ({name: "read_file", args: {path: n + ".txt", format: "json"}}));
                    const rs = tools.callBatch(entries);
                    const shape = rs.map(r => r instanceof Error
                        ? {k: r.kind, h: r.halted} : "ok");
                    var after = "ran";
                    try { tools.call("read_file", {path: "a.txt", format: "json"}); }
                    catch (e) { after = e.kind; }
                    return {shape, after};"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(
            v["shape"],
            json!([
                "ok", "ok", "ok",
                {"k": "budget", "h": true},
                {"k": "budget", "h": true}
            ]),
            "admitted prefix runs, tail refused with halted: {v}"
        );
        assert_eq!(
            v["after"],
            json!("budget"),
            "the latch holds for the rest of THIS execution: {v}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// impl-review #2: the budget refusal MESSAGE counts each entry exactly
    /// once. A multi-wave batch recomputes `used` from a dispatch-time
    /// snapshot; reading the live `calls.len()` there double-counts — by wave
    /// 2 it already holds this batch's finished traces while `admitted`
    /// counts the same entries again (the bug printed "(18 of 10)").
    #[test]
    fn batch_budget_message_counts_once_across_waves() {
        let dir = scratch();
        for n in 0..12 {
            std::fs::write(dir.join(format!("f{n}.txt")), "x").unwrap();
        }
        let cfg = CodeConfig {
            call_batch: true,
            max_tool_calls: 10,
            ..CodeConfig::default()
        };
        let tool = tool_with_tools(cfg, r#"["read_file","write_file","list_dir"]"#);
        let out = tool
            .run_with_emit(
                &json!({"code": r#"
                    var entries = [];
                    for (var i = 0; i < 12; i++)
                        entries.push({name: "read_file", args: {path: "f" + i + ".txt", format: "json"}});
                    const rs = tools.callBatch(entries);
                    return rs.map(r => r instanceof Error ? String(r.message) : "ok");"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        let arr = v.as_array().expect("array result");
        assert_eq!(arr.len(), 12, "{v}");
        assert_eq!(
            arr.iter().filter(|r| r.as_str() == Some("ok")).count(),
            10,
            "admitted prefix runs: {v}"
        );
        for r in &arr[10..] {
            let m = r.as_str().unwrap_or_default();
            assert!(
                m.contains("(10 of max_tool_calls=10)"),
                "refusal counts each entry once: {m}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §4 cancel: a cancel landing mid-batch → already-passed-gate unstarted
    /// entries complete as kind="cancel" halted:true entries (the 装配段补发
    /// discipline, batch form); the in-flight entry that tripped the flag
    /// still reports its own success.
    #[test]
    fn batch_cancel_mid_batch_refuses_tail() {
        struct TripCancel;
        impl Tool for TripCancel {
            fn def(&self) -> ToolDef {
                ToolDef {
                    name: "trip_cancel".into(),
                    description: "test: sets the run cancel flag".into(),
                    schema: json!({"type": "object", "properties": {}}),
                }
            }
            fn run(&self, _i: &Value, c: &ToolCtx) -> Result<ToolOutput> {
                if let Some(f) = &c.cancel {
                    f.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(ToolOutput::text("tripped"))
            }
            fn available_to_code(&self) -> bool {
                true
            }
            fn batchable(&self) -> bool {
                true
            }
        }
        let dir = scratch();
        std::fs::write(dir.join("a.txt"), "A").unwrap();
        std::fs::write(dir.join("b.txt"), "B").unwrap();
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":["read_file"],"code":{"enabled":true}}"#,
        )
        .unwrap();
        let tool = RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(batch_cfg())),
            &reqwest::blocking::Client::new(),
            "k",
            vec![],
            vec![Arc::new(Box::new(TripCancel))],
        );
        let mut c = ctx(&dir);
        c.cancel = Some(Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let out = tool
            .run_with_emit(
                &json!({"code": r#"
                    const rs = tools.callBatch([
                        {name: "trip_cancel", args: {}},
                        {name: "read_file", args: {path: "a.txt", format: "json"}},
                        {name: "read_file", args: {path: "b.txt", format: "json"}}
                    ]);
                    return rs.map(r => r instanceof Error
                        ? {k: r.kind, h: r.halted} : "ran");"#}),
                &c,
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(
            v,
            json!(["ran", {"k": "cancel", "h": true}, {"k": "cancel", "h": true}]),
            "in-flight entry completes, tail refused: {v}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-L4 并行（进程内真实线程版）: a Parallel batchable tool fanned out
    /// over one 8-entry wave MUST overlap in wall time — host-side (start,
    /// end) spans prove it; a batch that quietly serialized still passes
    /// every functional assertion, only the interval intersection catches
    /// it (the concurrency-needs-overlap-proof lesson).
    #[test]
    fn batch_parallel_wave_actually_overlaps() {
        use std::time::Duration;

        struct SlowProbe {
            spans: Arc<std::sync::Mutex<Vec<(Instant, Instant)>>>,
        }
        impl Tool for SlowProbe {
            fn def(&self) -> ToolDef {
                ToolDef {
                    name: "slow_probe".into(),
                    description: "test: sleeps then records its span".into(),
                    schema: json!({"type": "object", "properties": {
                        "tag": {"type": "string"}
                    }}),
                }
            }
            fn run(&self, _i: &Value, _c: &ToolCtx) -> Result<ToolOutput> {
                let s = Instant::now();
                std::thread::sleep(Duration::from_millis(120));
                self.spans.lock().unwrap().push((s, Instant::now()));
                Ok(ToolOutput::text("done"))
            }
            fn available_to_code(&self) -> bool {
                true
            }
            fn batchable(&self) -> bool {
                true
            }
            fn concurrency(&self) -> ToolConcurrency {
                ToolConcurrency::Parallel
            }
        }
        let dir = scratch();
        // One shared handle: the injected instance and the test's reader see
        // the same span log.
        let spans: Arc<std::sync::Mutex<Vec<(Instant, Instant)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let probe: Arc<Box<dyn Tool>> = Arc::new(Box::new(SlowProbe {
            spans: Arc::clone(&spans),
        }));
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":["read_file"],"code":{"enabled":true}}"#,
        )
        .unwrap();
        let tool = RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(batch_cfg())),
            &reqwest::blocking::Client::new(),
            "k",
            vec![],
            vec![probe],
        );
        let t0 = Instant::now();
        let out = tool
            .run_with_emit(
                &json!({"code": r#"
                    const entries = [1,2,3,4,5,6,7,8].map(i =>
                        ({name: "slow_probe", args: {tag: String(i)}}));
                    const rs = tools.callBatch(entries);
                    return rs.filter(r => r === "done").length;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v, json!(8), "all eight ran: {v}");
        let wall = t0.elapsed();
        let recorded = spans.lock().unwrap().clone();
        assert_eq!(recorded.len(), 8, "eight spans recorded");
        let mut overlaps = 0usize;
        for (i, (a_start, a_end)) in recorded.iter().enumerate() {
            for (j, (b_start, _)) in recorded.iter().enumerate() {
                if i != j && a_start < b_start && b_start < a_end {
                    overlaps += 1;
                }
            }
        }
        assert!(
            overlaps >= 2,
            "no interval overlap — the wave serialized: {overlaps} overlaps, wall {}ms",
            wall.as_millis()
        );
        assert!(
            wall < Duration::from_millis(8 * 120),
            "wall {}ms — a serial run would be ≥{}ms",
            wall.as_millis(),
            8 * 120
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// LD-19 保留名直调（gate ON half): `__call_batch` driven directly walks
    /// the exact same chain as the wrapper — the guest gets the raw envelope
    /// ARRAY (the wrapper's Error conversion is sugar on top).
    #[test]
    fn batch_reserved_name_direct_call_walks_full_chain() {
        let dir = scratch();
        std::fs::write(dir.join("a.txt"), "A").unwrap();
        let v = js_out(
            &batch_tool(),
            &dir,
            r#"
            const rs = tools.call("__call_batch", {entries: [
                {name: "read_file", args: {path: "a.txt", format: "json"}},
                {name: "write_file", args: {path: "x.txt", content: "no"}}
            ]});
            return {n: rs.length, first_ok: rs[0].__ok === true,
                    first_content_is_string: typeof rs[0].content === "string",
                    second_kind: rs[1].kind};"#,
        );
        assert_eq!(v["n"], json!(2), "{v}");
        assert_eq!(v["first_ok"], json!(true), "{v}");
        assert_eq!(
            v["first_content_is_string"],
            json!(true),
            "direct callers get the raw envelope array — per-entry auto-parse \
             is the wrapper's sugar, not the transport's: {v}"
        );
        assert_eq!(v["second_kind"], json!("batch"), "{v}");
        assert!(!dir.join("x.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// External design-review F1 regression: prep-refused entries still
    /// pair their ToolResult, land in the audit ledger, and consume quota —
    /// N denied entries must leave N in `calls.len()`, so post-batch single
    /// calls see the honest remaining budget (`max_tool_calls` is per
    /// execution, and the frozen copy promises failed calls count too).
    #[test]
    fn batch_refused_entries_pair_events_and_consume_quota() {
        use crate::hooks::{Hooks, ToolDecision};

        struct DenyAll;
        impl Hooks for DenyAll {
            fn before_tool(&self, _t: &str, _i: &Value, _c: &ToolCtx) -> ToolDecision {
                ToolDecision::Deny("no reads today".into())
            }
        }
        let dir = scratch();
        std::fs::write(dir.join("a.txt"), "A").unwrap();
        let cfg = CodeConfig {
            call_batch: true,
            max_tool_calls: 2,
            ..CodeConfig::default()
        };
        let tool = tool_with_tools(cfg, r#"["read_file","list_dir"]"#);
        let mut c = ctx(&dir);
        c.hooks = Some(Arc::new(DenyAll));
        let mut events: Vec<AgentEvent> = Vec::new();
        let out = tool
            .run_with_emit(
                &json!({"code": r#"
                    const rs = tools.callBatch([
                        {name: "read_file", args: {path: "a.txt", format: "json"}},
                        {name: "read_file", args: {path: "a.txt", format: "json"}},
                        {name: "read_file", args: {path: "a.txt", format: "json"}}
                    ]);
                    const shape = rs.map(r => r instanceof Error
                        ? {k: r.kind, h: r.halted === undefined ? null : r.halted} : "ok");
                    var after = "ran";
                    try { tools.call("read_file", {path: "a.txt", format: "json"}); }
                    catch (e) { after = e.kind; }
                    return {shape, after};"#}),
                &c,
                &mut |ev| events.push(ev),
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(
            v["shape"],
            json!([
                {"k": "hooks", "h": null},
                {"k": "hooks", "h": null},
                {"k": "budget", "h": true}
            ]),
            "two denied (admitted) entries + one over-budget tail: {v}"
        );
        assert_eq!(
            v["after"],
            json!("budget"),
            "the denied entries consumed the execution's quota: {v}"
        );
        // Event pairing: every ANNOUNCED entry has its ToolResult (the
        // budget tail was refused before announcing, so it stays silent).
        let calls = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCall { .. }))
            .count();
        let results = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolResult { .. }))
            .count();
        assert_eq!((calls, results), (2, 2), "announced entries pair 1:1");
        // And the audit ledger agrees with the announcements.
        let meta = out.meta.expect("audit meta");
        assert_eq!(meta["call_count"], 2, "{meta}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whole-call shape: a malformed BATCH (not the entries) throws
    /// kind="batch" to the guest instead of returning an array.
    #[test]
    fn batch_whole_call_shape_error_throws() {
        let out = batch_tool()
            .run_with_emit(
                &json!({"code": r#"
                    try { tools.callBatch(null); return {threw: false}; }
                    catch (e) { return {threw: true, kind: e.kind}; }"#}),
                &ctx(&scratch()),
                &mut |_| {},
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["threw"], json!(true), "{}", out.content);
        assert_eq!(v["kind"], json!("batch"), "{}", out.content);
    }

    /// LD-15 gate-ON description: the 【批量】 overlay names the wrapper and
    /// the three no-transaction sentences' home (the skill), stays bounded,
    /// and composes with the 【库】 overlay without touching the base.
    #[test]
    fn gate_on_description_lists_batch_helper() {
        let d = tool_with(batch_cfg()).def().description;
        assert!(d.contains("【批量】"), "{d}");
        assert!(d.contains("tools.callBatch"), "{d}");
        assert!(d.contains("无原子性、无回滚、部分执行是正常结果"), "{d}");
        assert!(!d.contains("【库】"), "the other gate stays off: {d}");
        assert!(
            d.chars().count() <= 1600,
            "gate-on description grew to {} chars",
            d.chars().count()
        );
        let both = tool_with(CodeConfig {
            include: true,
            call_batch: true,
            ..CodeConfig::default()
        })
        .def()
        .description;
        assert!(
            both.contains("【库】") && both.contains("【批量】"),
            "{both}"
        );
        assert!(
            both.chars().count() <= 1800,
            "both-gates description grew to {} chars",
            both.chars().count()
        );
        // And the prelude exposes the wrapper only when on.
        let out = run_code(r#"return typeof tools.callBatch;"#, batch_cfg());
        assert_eq!(out.content, "\"function\"");
    }
}
