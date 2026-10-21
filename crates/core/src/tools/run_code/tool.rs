//! The run_code tool itself: `RunCodeTool`, its lazily-built registry, the
//! eval loop, and the `Tool` impl.

use super::*;

pub(super) fn guest_ctx(ctx: &ToolCtx) -> ToolCtx {
    ToolCtx {
        tool_absent_notes: ctx.tool_absent_notes.clone(),
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
    /// Shared provider cell for the `llm` primitive (LD-16): late-filled by
    /// `Agent::set_code_tool` with the SAME `Arc<dyn Provider>` the outer
    /// agent chats through. Must be an Arc'd cell (not a plain field) so
    /// every clone of the lineage — and the `LlmTool` the registry builds —
    /// sees the one fill; a plain OnceLock would split between clones and
    /// guests would call an unfilled copy.
    provider_cell: Arc<OnceLock<Arc<dyn crate::provider::Provider>>>,
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
            provider_cell: Arc::new(OnceLock::new()),
        }
    }

    /// The shared provider cell (LD-16) — the fill point `Agent::
    /// set_code_tool` writes through. `pub(crate)` so the agent (same
    /// crate) is the only writer; guests only ever read it through the
    /// `LlmTool` the registry builds.
    pub(crate) fn provider_cell(&self) -> &Arc<OnceLock<Arc<dyn crate::provider::Provider>>> {
        &self.provider_cell
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
                // The `llm` primitive (LD-1/LD-14): admitted iff the
                // snapshot AT THIS BUILD has the gate on — the OnceLock
                // caches the decision for the lineage's lifetime, so a
                // later hot flip is not sensed (documented snapshot
                // boundary, same class as the MCP hot add/remove).
                if self.snapshot().llm.enabled {
                    // LD-3 name reservation, guest side: an admitted tool
                    // already named "llm" (an MCP server, an injected
                    // opt-in) must not be silently shadowed — dispatch is
                    // first-match, the model would be calling a fake llm.
                    // The whole registry build fails (cached, reported on
                    // every call), mirroring validate_tool_names' Err+hint
                    // on the top-level side.
                    if tools.iter().any(|t| t.def().name == "llm") {
                        return Err(anyhow::anyhow!(
                            "a tool named `llm` is already registered (an MCP server or an \
                             injected tool) — code.llm.enabled=true reserves that name for \
                             the sandbox llm primitive; rename the tool or set \
                             code.llm.enabled=false"
                        ));
                    }
                    tools.push(Box::new(crate::tools::llm::LlmTool::new(
                        self.snapshot().llm.clone(),
                        Arc::clone(&self.provider_cell),
                    )));
                }
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
                meta: Some(audit_meta(&[], &snap, 0, false, 0, &LlmAudit::default())),
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
            read_memory_string(&mem, &mut store, p, l)
        } else {
            let packed = match instance
                .get_typed_func::<(), i64>(&mut store, "take_last_error")
                .and_then(|f| f.call(&mut store, ()))
            {
                Ok(p) => p,
                Err(trap) => return self.terminated(&mut store, &snap, trap),
            };
            let (p, l) = unpack(packed);
            read_memory_string(&mem, &mut store, p, l)
        };

        let logs = std::mem::take(&mut store.data_mut().logs);
        let usage = std::mem::take(&mut store.data_mut().usage);
        // Over-cap read: the guest DID produce a result (or error text) — it
        // serialized something larger than the host's single-read cap into
        // linear memory it owns. The old `.unwrap_or_default()` folded this
        // into is_error=false with content "" (and an empty error message on
        // the rc!=0 leg) — silent data loss with nothing telling the model
        // the result existed. Both legs must be loud instead, in the same
        // voice as the terminated/truncation messages: the run finished, the
        // host refused the copy, and the recovery is to stop RETURNING data
        // that big and page through a file instead.
        let read_capped = outcome.is_none();
        let mut value = match outcome {
            Some(s) => s,
            None if rc == 0 => format!(
                "[run_code] result exceeds the {} MiB host read cap — write it \
                 to a file (write_file) and read a slice instead (read_file \
                 offset/limit)",
                GUEST_READ_CAP_BYTES / (1024 * 1024)
            ),
            None => format!(
                "[run_code] JS error: (message exceeds the {} MiB host read cap)",
                GUEST_READ_CAP_BYTES / (1024 * 1024)
            ),
        };
        if rc != 0 && !read_capped {
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
            &store.data().llm,
        );
        ToolOutput {
            // A capped read is an error even on rc==0: the guest's actual
            // outcome was "returned something enormous", and the honest
            // report of that is the cap message above, never a success.
            is_error: rc != 0 || read_capped,
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
            &store.data().llm,
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
        let mut description = base.description;
        if snap.include {
            description.push_str(LIB_DESC_SUFFIX);
        }
        if snap.call_batch {
            // M3 revisit (recorded in the design revision log): appendix ④'s
            // canonical reader list is "read_file / llm" — llm is named only
            // when ITS gate is also on at def() time, so an llm-off config
            // keeps the closed-list contract honest (no phantom tool in the
            // description). The splice targets the suffix const, never the
            // pinned base literal above.
            let suffix = if snap.llm.enabled {
                BATCH_DESC_SUFFIX.replace(
                    "read_file 等 batchable 工具",
                    "read_file / llm 等 batchable 工具",
                )
            } else {
                BATCH_DESC_SUFFIX.to_string()
            };
            description.push_str(&suffix);
        }
        if snap.llm.enabled {
            description.push_str(LLM_DESC_SUFFIX);
        }
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

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
    fn missing_code_field_is_tool_error() {
        let out = tool_with(CodeConfig::default())
            .run_with_emit(&json!({}), &ctx(&scratch()), &mut |_| {})
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("missing string field 'code'"));
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

    /// Host read cap, success leg: a return value whose serialized form
    /// exceeds GUEST_READ_CAP_BYTES (16 MiB) must surface as a LOUD error
    /// naming the cap and the file-slice recovery — the old
    /// `.unwrap_or_default()` folded it into is_error=false with content ""
    /// (silent data loss: the guest computed 17 MiB, the model was told
    /// "" and had no way to know anything was dropped). 17 MiB fits the
    /// default guest memory (mem_mb=256), so the run itself succeeds and
    /// the cap is the only thing standing between the guest and the host.
    #[test]
    fn oversized_return_value_is_a_loud_read_cap_error_not_empty_success() {
        let out = run_code(
            r#"return "x".repeat(17 * 1024 * 1024);"#,
            CodeConfig::default(),
        );
        assert!(out.is_error, "over-cap result must be an error");
        assert!(
            out.content
                .contains("result exceeds the 16 MiB host read cap"),
            "names the cap: {}",
            out.content
        );
        assert!(
            out.content.contains("write it to a file")
                && out.content.contains("read a slice instead"),
            "recovery hint rides along: {}",
            out.content
        );
        // And the cap actually bounded the host copy — the 17 MiB never
        // rides into the output.
        assert!(
            out.content.chars().count() < 1000,
            "message stays short: {}",
            out.content.len()
        );
    }

    /// Host read cap, error leg: a JS error whose MESSAGE exceeds the cap
    /// gets an explicit placeholder instead of an empty error string — the
    /// guest failed loudly, the host must not swallow the shape of that
    /// failure into `JS error: ` with nothing after the colon.
    #[test]
    fn oversized_js_error_message_gets_a_read_cap_placeholder() {
        let out = run_code(
            r#"throw new Error("e".repeat(17 * 1024 * 1024));"#,
            CodeConfig::default(),
        );
        assert!(out.is_error);
        assert_eq!(
            out.content, "[run_code] JS error: (message exceeds the 16 MiB host read cap)",
            "explicit placeholder, not an empty message"
        );
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
}
