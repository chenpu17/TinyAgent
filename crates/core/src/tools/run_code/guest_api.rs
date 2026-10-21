//! The host half of the guest API surface: `env.host_call_tool` dispatch,
//! reserved-name routing, guest memory I/O, result envelopes, audit folds.

use super::*;

/// Reserved tool name the guest's `tools.list()` routes to the host (§5.2).
/// Interception happens BEFORE the hooks gate, matching the design's
/// interception order (wall clock → reserved name → hooks → deny → run).
pub(super) const TOOLS_LIST_NAME: &str = "__tools_list";
/// Reserved name behind `tools.budget()` (v2 review): read-only limits
/// introspection — calls left, wall clock left, fuel left. Same dispatch
/// class as [`TOOLS_LIST_NAME`] (no gate/audit/quota), so checking the
/// budget never spends it.
pub(super) const BUDGET_NAME: &str = "__budget";
/// Reserved names of the lib family (code_memory_design MD-2, M1). Unlike
/// the two above these are REAL gated actions — host file reads under
/// policy — dispatched through [`lib_reserved_call`]: lib/ confinement →
/// deny_paths → hooks gate (pseudo-tool name without the dunder) → cancel
/// checkpoint → host work, audited and counted like any inner call. The
/// prelude wrappers `include()` / `saveLib()` route here; the guest may
/// also call the names directly (same checks either way).
pub(super) const INCLUDE_NAME: &str = "__include";
pub(super) const HASH_FILE_NAME: &str = "__hash_file";
/// Reserved names of saveLib's INDEX step (external re-review P1/P2). Unlike
/// the gated lib family above, both are read-only/introspection class (same
/// class as [`BUDGET_NAME`] — no gate/audit/quota): `__lib_index_state`
/// reports whether [`LIB_INDEX_REL`] exists (a filesystem fact, NOT parsed
/// out of an error string — a hooks denial whose text happens to contain an
/// OS "not found" phrasing cannot spoof the empty-index initialization),
/// and `__lib_update_index` is a PURE text-in/text-out merge (the upsert)
/// validated by the same line parser the admission uses — one parser, no
/// guest/host drift. The real IO around them stays with the ordinary gated
/// tools: saveLib reads INDEX via `read_file` (hooks can deny) and writes it
/// via `write_file` (hooks can deny/modify).
pub(super) const LIB_INDEX_STATE_NAME: &str = "__lib_index_state";
pub(super) const LIB_UPDATE_INDEX_NAME: &str = "__lib_update_index";
/// Reserved name behind `tools.callBatch(entries)` (code_llm_design §4/LD-19,
/// M2): one batched dispatch of read-only calls. The reserved name is only
/// the ENTRANCE — every entry runs the exact single-call sequence (entry
/// checks → gate → deny/fs → run → after); nothing is exempted.
pub(super) const CALL_BATCH_NAME: &str = "__call_batch";
/// Guest-visible tombstone for "cancel observed, the sandbox is halting" —
/// shared by the host_call ENTRY gate (§13-5) and the post-hooks-gate cancel
/// checkpoint so the copy stays single-sourced (LLM-visible copy is spec;
/// see docs/finetune_support_design.md appendix A).
pub(super) const CANCEL_HALTED_WHY: &str =
    "run_code: cancelled (host cancel flag) — the sandbox is halted for the rest of this execution";

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
pub(super) struct Registry {
    pub(super) tools: Arc<Vec<Box<dyn Tool>>>,
    pub(super) defs: Arc<Vec<ToolDef>>,
}

// ── import bodies ──────────────────────────────────────────────────────────

/// Split a packed `(ptr << 32) | len` return from the guest.
pub(super) fn unpack(packed: i64) -> (usize, usize) {
    ((packed >> 32) as u32 as usize, packed as u32 as usize)
}

/// Read a byte range the guest points at. `None` on null/oversized/OOB — the
/// guest is untrusted, so every pointer/length pair is validated before use.
pub(super) fn read_guest(
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
pub(super) fn write_guest(caller: &mut Caller<'_, HostCtx>, bytes: &[u8]) -> Result<i64> {
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
pub(super) fn parse_guest_args(raw: &str) -> Result<Value, String> {
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
pub(super) fn call_limit_exceeded_why(used: usize, max: u32) -> String {
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
pub(super) fn fold_inner_usage(d: &mut HostCtx, output: &ToolOutput) {
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

/// The guest envelope's error CLASS for a real tool run (LD-7): a tool with
/// its own failure vocabulary (`Tool::envelope_error_kind` — the `llm`
/// primitive's "llm" / "llm_output" / "budget") names it; every other tool
/// is plain "tool" (or "unknown_tool" when the name never resolved — the
/// caller's `known` flag). The by-name lookup runs on the ERROR path only;
/// the happy path pays nothing.
pub(super) fn real_run_class(
    tools: &[Box<dyn Tool>],
    name: &str,
    known: bool,
    output: &ToolOutput,
) -> &'static str {
    if output.is_error {
        if let Some(kind) = tools
            .iter()
            .find(|t| t.def().name == name)
            .and_then(|t| t.envelope_error_kind(output))
        {
            return kind;
        }
    }
    if known {
        "tool"
    } else {
        "unknown_tool"
    }
}

/// Fold one output's `llm_call` audit marker (LD-12) into the execution's
/// ledger. Called from the same completion tails as [`fold_inner_usage`] —
/// the single-call Phase B tail and every batch completion — so failure
/// exits carry their cost story too ("完整性信息必须在成功和失败出口都可见").
/// Saturating everywhere: a lying tool meta cannot wrap the ledger.
pub(super) fn fold_llm_audit(d: &mut HostCtx, output: &ToolOutput) {
    let Some(row) = output.meta.as_ref().and_then(|m| m.get("llm_call")) else {
        return;
    };
    d.llm.calls = d.llm.calls.saturating_add(1);
    let reserved = row.get("reserved").and_then(Value::as_u64).unwrap_or(0);
    d.llm.reserved = d.llm.reserved.saturating_add(reserved);
    let model = row
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();
    let m = d.llm.models.entry(model).or_default();
    match row.get("actual").and_then(Value::as_u64) {
        Some(a) => {
            m.known_usage = m.known_usage.saturating_add(a);
            m.known_calls = m.known_calls.saturating_add(1);
            d.llm.settled = d.llm.settled.saturating_add(a);
        }
        None => m.unknown_calls = m.unknown_calls.saturating_add(1),
    }
    if row.get("over_reserve").and_then(Value::as_bool) == Some(true) {
        d.llm.over_reserve = d.llm.over_reserve.saturating_add(1);
    }
}

pub(super) fn host_call_tool(
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

    if name == LIB_INDEX_STATE_NAME {
        // saveLib's existence probe (external re-review P1): classification
        // by FILESYSTEM FACT, never by parsing an error string — a hooks
        // denial whose text happens to contain an OS "not found" phrasing
        // cannot spoof the empty-index initialization. Read-only
        // introspection class (same as `__budget` above): no gate/audit/
        // quota; the actual INDEX read/write stay with the ordinary gated
        // file tools the prelude calls.
        let denied = {
            let d = caller.data();
            if !d.snap.include {
                Some(format!(
                    "run_code: {name} is not available in this configuration \
                     (code.include=false) — the lib helpers are disabled"
                ))
            } else {
                None
            }
        };
        if let Some(msg) = denied {
            return envelope_err(caller, "denied", Some(name.as_str()), Some(false), &msg);
        }
        let payload = {
            let d = caller.data();
            match std::fs::metadata(d.workdir_canon.join(LIB_INDEX_REL)) {
                Ok(_) => json!({ "state": "exists" }),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    json!({ "state": "missing" })
                }
                Err(e) => {
                    return envelope_err(
                        caller,
                        "lib",
                        Some(name.as_str()),
                        Some(false),
                        &format!("run_code: cannot stat {LIB_INDEX_REL}: {e}"),
                    )
                }
            }
        };
        return envelope_or_trap(caller, true, &payload.to_string());
    }

    if name == LIB_UPDATE_INDEX_NAME {
        // saveLib's merge (external re-review P1/P2): a PURE text→text
        // upsert validated by the SAME line parser the admission uses
        // (`parse_lib_line`) — one grammar, no guest/host drift. The IO
        // stays with the caller: the prelude reads INDEX through the gated
        // read_file (hooks can deny) and writes the returned text through
        // the gated write_file. Read-only class like `__budget`.
        let denied = {
            let d = caller.data();
            if !d.snap.include {
                Some(format!(
                    "run_code: {name} is not available in this configuration \
                     (code.include=false) — the lib helpers are disabled"
                ))
            } else {
                None
            }
        };
        if let Some(msg) = denied {
            return envelope_err(caller, "denied", Some(name.as_str()), Some(false), &msg);
        }
        let merged = {
            let (rel, sha, text) = (
                args.get("rel").and_then(Value::as_str),
                args.get("sha256").and_then(Value::as_str),
                args.get("text").and_then(Value::as_str),
            );
            match (rel, sha, text) {
                (Some(r), Some(s), Some(t)) => lib_upsert_index(r, s, t),
                _ => Err(format!(
                    "run_code: {name} needs string fields 'rel', 'sha256' and 'text'"
                )),
            }
        };
        // The merged text rides inside an object on purpose: `content` is
        // auto-JSON-parsed guest-side, and a degenerate INDEX whose whole
        // text parses as JSON must still arrive as the STRING it is.
        return match merged {
            Ok(s) => envelope_or_trap(caller, true, &json!({ "index": s }).to_string()),
            Err(e) => envelope_err(caller, "lib", Some(name.as_str()), Some(false), &e),
        };
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
                            let class = real_run_class(&d.tools, &name, known, &output);
                            (output, class)
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
        fold_llm_audit(d, &output);
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
        // LD-7: "tool" AND the llm classes deliberately OMIT `retryable` —
        // the host cannot classify a plain tool's own failure, and an llm
        // failure's retry-worthiness (transient 5xx vs deterministic param
        // reject) is the model's judgment from the message. Every other
        // class is definitive → retryable:false.
        let retryable = if matches!(err_class, "tool" | "llm" | "llm_output") {
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
pub(super) fn emit_inner_result(caller: &mut Caller<'_, HostCtx>, name: &str, output: &ToolOutput) {
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
pub(super) fn list_entry(def: &ToolDef) -> Value {
    let head: String = def.description.chars().take(200).collect();
    json!({ "name": def.name, "description": head })
}

/// Compact-JSON length + truncated sha256 (first 16 bytes, hex) for the audit
/// trace — the 128-bit prefix is beyond any practical collision effort, and
/// the full plaintext already lives in the event stream (§8.4's 明文口径:
/// "not plaintext" applies to result.json only, which these hashes harden).
pub(super) fn hash_input(v: &Value) -> (usize, String) {
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
pub(super) fn assemble_with_logs(mut value: String, logs: &[String], cap: usize) -> (String, bool) {
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

pub(super) fn audit_meta(
    calls: &[CallTrace],
    snap: &CodeConfig,
    fuel_used: u64,
    truncated: bool,
    logs_dropped: u64,
    llm: &LlmAudit,
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
    // LD-12 llm ledger — additive key, only when the primitive actually ran
    // (llm-off / no-llm-call runs stay byte-identical). Per-model known
    // usage + unknown-call counts; `settled` is KNOWN usage, never
    // presented as the complete bill when `unknown_calls` > 0.
    if llm.calls > 0 {
        let models: serde_json::Map<String, Value> = llm
            .models
            .iter()
            .map(|(m, s)| {
                (
                    m.clone(),
                    json!({
                        "known_usage": s.known_usage,
                        "known_calls": s.known_calls,
                        "unknown_calls": s.unknown_calls,
                    }),
                )
            })
            .collect();
        meta["llm"] = json!({
            "calls": llm.calls,
            "reserved": llm.reserved,
            "settled": llm.settled,
            "over_reserve": llm.over_reserve,
            "models": models,
        });
    }
    meta
}

/// The fallback envelope's error text for an undeliverable tool result —
/// SIZE ONLY, never the payload. The payload is precisely the string that
/// just failed to fit in the guest, so re-embedding it in the retry would
/// re-allocate the same peak (in the batch case the ENTIRE result array)
/// for a write that is about to fail the same way — pure amplification on
/// the OOM path. The byte count is the diagnosis (an allocation of that
/// size failed); the bytes themselves are noise. Extracted as a pure
/// function so the fixed short shape is unit-testable — the live path needs
/// a guest alloc to fail mid-call, which cannot be injected honestly from a
/// host test.
pub(super) fn undeliverable_result_msg(payload_len: usize) -> String {
    format!(
        "run_code: host could not return the tool result (size {payload_len} bytes — the \
         guest could not allocate room for it; likely memory or fuel exhaustion)"
    )
}

/// Write the envelope into the guest, or fabricate a same-shaped error
/// envelope in host memory if even that fails (allocation trapped / OOM) —
/// the guest contract requires SOME (ptr,len) back from every call.
pub(super) fn envelope_or_trap(
    caller: &mut Caller<'_, HostCtx>,
    is_ok: bool,
    payload: &str,
) -> i64 {
    let bytes = if is_ok {
        json!({ "__ok": true, "content": payload }).to_string()
    } else {
        json!({ "__ok": false, "error": payload }).to_string()
    };
    match write_guest(caller, bytes.as_bytes()) {
        Ok(packed) => packed,
        Err(_) => {
            // alloc trapped (fuel/memory exhausted mid-call) — mirror the
            // failure as a SHORT same-shaped error envelope so the guest
            // exits with the right diagnosis instead of a bogus result. The
            // old fallback re-embedded the full payload here, doubling the
            // peak allocation exactly when the guest has no room.
            let fallback =
                json!({ "__ok": false, "error": undeliverable_result_msg(payload.len()) })
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
pub(super) fn envelope_err(
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

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
                "{}/../../examples/mcp_echo.py",
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

    /// The undeliverable-result fallback text is SIZE-ONLY by construction:
    /// it stays short for an arbitrarily large payload. The old format!
    /// re-embedded the payload itself — the amplification this function
    /// exists to remove — so the shape (not just the wording) is pinned
    /// here; the live path needs a guest alloc to fail mid-call, which the
    /// e2e below stages through a real heap exhaustion instead.
    #[test]
    fn undeliverable_result_msg_is_short_and_names_the_size() {
        let small = undeliverable_result_msg(10);
        let huge = undeliverable_result_msg(20_000_000);
        assert!(
            small.starts_with("run_code: host could not return the tool result (size "),
            "{small}"
        );
        assert!(small.contains("10 bytes"), "{small}");
        assert!(huge.contains("20000000 bytes"), "{huge}");
        assert!(huge.len() < 300, "fixed short shape: {}", huge.len());
    }

    // ── code_llm M3: llm 原语 ─────────────────────────────────────────────
    //
    // A-L 编号对应 docs/code_llm_design.md §7；A-L4 的 e2e 时间区间重叠
    // 另在 scripts/e2e_scenarios.py，这里用进程内真实线程 + 区间记录钉住
    // 并发本身（内存纪律：功能单测证明不了真并发）。

    /// A-L1: the minimal loop — one llm call through the real guest dispatch,
    /// billed EXACTLY once (settle replaces the reservation; the fold merges
    /// totals without re-sharing), audit row landed, request shaped per LD-4.
    #[test]
    fn llm_minimal_loop_bills_once_and_settles_once() {
        let dir = scratch();
        let (prov, state) =
            crate::provider::mock::MockProvider::new(vec![crate::provider::mock::resp_text(
                "yes",
                crate::provider::mock::mk_usage(10, 5),
            )]);
        let g = gated_ctx(&dir, 10_000);
        let out = llm_tool(llm_cfg(), std::sync::Arc::new(prov))
            .run_with_emit(
                &json!({"code": r#"
                    const r = tools.call("llm", {prompt: "hello"});
                    return { text: r.text, model: r.model, in: r.usage.input_tokens };"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["text"], json!("yes"), "{}", out.content);
        assert_eq!(v["model"], json!("m1"), "LD-5 default_model route");
        assert_eq!(v["in"], json!(10), "LD-6 real provider usage passthrough");
        // LD-17 settle-once: the reservation was est("hello")=1+64 +1024 =
        // 1089; settle(15) refunds 1074 — the counter holds the ACTUAL 15.
        assert_eq!(
            g.budget
                .as_ref()
                .unwrap()
                .used
                .load(std::sync::atomic::Ordering::Relaxed),
            15,
            "settle replaced the reservation with actual usage"
        );
        // One provider call, shaped per LD-4: temperature pinned at the
        // provider layer, default max_tokens, the model the tier picked.
        let cap = state.captured();
        assert_eq!(cap.len(), 1);
        assert_eq!(cap[0].temperature, Some(0.0));
        assert_eq!(cap[0].max_tokens, 1024);
        assert_eq!(cap[0].model, "m1");
        assert!(!cap[0].cache, "one-shot, no cache-write premium");
        // Usage folds ONCE into run_code's aggregate (self-settled — flag
        // true means the parent fold must not re-share the counter half).
        assert_eq!(out.usage.as_ref().map(|u| u.total()), Some(15));
        assert!(out.usage_in_shared_budget);
        // LD-12 audit: known usage attributed to the model.
        let llm = out.meta.as_ref().unwrap().get("llm").unwrap();
        assert_eq!(llm["calls"], json!(1));
        assert_eq!(llm["settled"], json!(15));
        assert_eq!(llm["models"]["m1"]["known_usage"], json!(15));
        assert_eq!(llm["models"]["m1"]["unknown_calls"], json!(0));
    }

    /// A-L2b: usage never arriving is NOT zero — the reservation stands in
    /// the budget, the totals stay unfaked, the audit says unknown.
    #[test]
    fn llm_missing_usage_keeps_reservation_and_fakes_nothing() {
        let dir = scratch();
        let (prov, _) =
            crate::provider::mock::MockProvider::new(vec![crate::provider::mock::resp_text(
                "yes", None,
            )]);
        let g = gated_ctx(&dir, 10_000);
        let out = llm_tool(llm_cfg(), std::sync::Arc::new(prov))
            .run_with_emit(
                &json!({"code": r#"
                    const r = tools.call("llm", {prompt: "hi"});
                    return { text: r.text, usage: r.usage };"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(
            v["usage"],
            json!(null),
            "LD-6: usage is null, not fabricated"
        );
        // Reservation kept: est("hi") = 0 + 64 + 1024 = 1088 still held.
        assert_eq!(
            g.budget
                .as_ref()
                .unwrap()
                .used
                .load(std::sync::atomic::Ordering::Relaxed),
            1088,
            "unknown usage keeps the full reservation (over-hold, never under-hold)"
        );
        assert_eq!(
            out.usage.map(|u| u.total()).unwrap_or(0),
            0,
            "no fake usage in the aggregate either"
        );
        let llm = out.meta.as_ref().unwrap().get("llm").unwrap();
        assert_eq!(llm["models"]["m1"]["unknown_calls"], json!(1));
        assert_eq!(llm["settled"], json!(0));
        assert_eq!(llm["reserved"], json!(1088));
    }

    /// A-L2c, labels-failure arm: an out-of-vocabulary output still settles
    /// and still bills (非法输出仍有成本) — kind="llm_output", usage folds.
    #[test]
    fn llm_label_miss_settles_bills_and_reports_llm_output() {
        let dir = scratch();
        let (prov, _) =
            crate::provider::mock::MockProvider::new(vec![crate::provider::mock::resp_text(
                "MAYBE SO",
                crate::provider::mock::mk_usage(7, 9),
            )]);
        let g = gated_ctx(&dir, 10_000);
        let out = llm_tool(llm_cfg(), std::sync::Arc::new(prov))
            .run_with_emit(
                &json!({"code": r#"
                    try {
                        tools.call("llm", {prompt: "pick one", labels: ["yes", "no"]});
                        return { threw: false };
                    } catch (e) {
                        return { threw: true, kind: e.kind, retryable: e.retryable };
                    }"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["threw"], json!(true), "{}", out.content);
        assert_eq!(v["kind"], json!("llm_output"), "{}", out.content);
        assert_eq!(
            v["retryable"],
            Value::Null,
            "LD-7: llm classes omit retryable (the model judges)"
        );
        // Settled to actual (16) and billed through the fold.
        assert_eq!(
            g.budget
                .as_ref()
                .unwrap()
                .used
                .load(std::sync::atomic::Ordering::Relaxed),
            16
        );
        assert_eq!(out.usage.map(|u| u.total()), Some(16));
        let llm = out.meta.as_ref().unwrap().get("llm").unwrap();
        assert_eq!(
            llm["settled"],
            json!(16),
            "the failed-validation call is audited"
        );
    }

    /// A-L2c, request-error arm: a failed provider request keeps the
    /// reservation, reports kind="llm", omits retryable.
    #[test]
    fn llm_request_error_keeps_reserve_and_kinds_llm() {
        let dir = scratch();
        let (prov, _) =
            crate::provider::mock::MockProvider::new(vec![crate::provider::mock::MockTurn::Err(
                "gateway 503".into(),
            )]);
        let g = gated_ctx(&dir, 10_000);
        let out = llm_tool(llm_cfg(), std::sync::Arc::new(prov))
            .run_with_emit(
                &json!({"code": r#"
                    try {
                        tools.call("llm", {prompt: "hi"});
                        return { threw: false };
                    } catch (e) {
                        return { threw: true, kind: e.kind, retryable: e.retryable };
                    }"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["threw"], json!(true), "{}", out.content);
        assert_eq!(v["kind"], json!("llm"), "{}", out.content);
        assert_eq!(v["retryable"], Value::Null, "{}", out.content);
        assert_eq!(
            g.budget
                .as_ref()
                .unwrap()
                .used
                .load(std::sync::atomic::Ordering::Relaxed),
            1088,
            "dispatched-and-failed keeps the reservation (LD-17 row 2)"
        );
        let llm = out.meta.as_ref().unwrap().get("llm").unwrap();
        assert_eq!(llm["models"]["m1"]["unknown_calls"], json!(1));
    }

    /// A-L2c, provider-panic arm: the Drop-settlement guarantee — a panic
    /// mid-flight unwinds through everything, and the reservation survives
    /// (the guard's Drop keeps it; no under-hold).
    #[test]
    fn llm_provider_panic_keeps_reservation_via_drop() {
        struct PanicProvider;
        impl crate::provider::Provider for PanicProvider {
            fn chat(
                &self,
                _req: &crate::provider::ChatRequest,
                _on_event: &mut dyn FnMut(crate::provider::ChatEvent),
            ) -> anyhow::Result<crate::provider::ChatResponse> {
                panic!("custom provider panicked mid-call");
            }
        }
        let dir = scratch();
        let g = gated_ctx(&dir, 10_000);
        let tool = llm_tool(llm_cfg(), std::sync::Arc::new(PanicProvider));
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tool.run_with_emit(
                &json!({"code": r#"tools.call("llm", {prompt: "hi"}); return 1;"#}),
                &g,
                &mut |_| {},
            )
        }));
        assert!(r.is_err(), "the panic must propagate (no silent swallow)");
        assert_eq!(
            g.budget
                .as_ref()
                .unwrap()
                .used
                .load(std::sync::atomic::Ordering::Relaxed),
            1088,
            "ReserveGuard::Drop kept the full reservation through the unwind"
        );
    }

    /// External audit P1-1: a ChatResponse that is Ok at the transport
    /// layer but carries an abort-vocabulary stop_reason is NOT a success —
    /// kind="llm" naming the stop reason, the partial text rides the
    /// message, the partial usage settles honestly (the truncated
    /// generation's cost is real), and label validation never runs (a
    /// truncated "yes" must not pass as a verdict).
    #[test]
    fn llm_stream_error_settles_partial_usage_and_kinds_llm() {
        let dir = scratch();
        let (prov, _) =
            crate::provider::mock::MockProvider::new(vec![crate::provider::mock::MockTurn::Ok(
                crate::provider::ChatResponse {
                    blocks: vec![crate::provider::Block::Text("par".into())],
                    stop_reason: "stream_error".into(),
                    usage: crate::provider::mock::mk_usage(10, 5),
                },
            )]);
        let g = gated_ctx(&dir, 10_000);
        let out = llm_tool(llm_cfg(), std::sync::Arc::new(prov))
            .run_with_emit(
                &json!({"code": r#"
                    try {
                        tools.call("llm", {prompt: "hi", labels: ["yes", "no"]});
                        return { threw: false };
                    } catch (e) {
                        return { threw: true, kind: e.kind, msg: String(e) };
                    }"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["threw"], json!(true), "{}", out.content);
        assert_eq!(
            v["kind"],
            json!("llm"),
            "termination, not vocabulary: {}",
            out.content
        );
        let msg = v["msg"].as_str().unwrap_or_default();
        assert!(
            msg.contains("stream_error") && msg.contains("par"),
            "stop reason + partial text ride the message: {msg}"
        );
        // Settled against the PARTIAL usage (15): the reservation was
        // replaced by actual — an abnormal end is still a billed call.
        assert_eq!(
            g.budget
                .as_ref()
                .unwrap()
                .used
                .load(std::sync::atomic::Ordering::Relaxed),
            15
        );
        assert_eq!(out.usage.map(|u| u.total()), Some(15));
        let llm = out.meta.as_ref().unwrap().get("llm").unwrap();
        assert_eq!(llm["settled"], json!(15));
        assert_eq!(llm["models"]["m1"]["known_calls"], json!(1));
        assert_eq!(llm["models"]["m1"]["unknown_calls"], json!(0));
    }

    /// P1-1, unknown-usage arm: an abnormally-ended response with NO usage
    /// keeps the reservation (over-hold, never under-hold) and audits an
    /// unknown call — the same LD-17 row-2 shape as a request error.
    #[test]
    fn llm_stream_truncated_without_usage_keeps_reservation() {
        let dir = scratch();
        let (prov, _) =
            crate::provider::mock::MockProvider::new(vec![crate::provider::mock::MockTurn::Ok(
                crate::provider::ChatResponse {
                    blocks: vec![crate::provider::Block::Text("partial".into())],
                    stop_reason: "stream_truncated".into(),
                    usage: None,
                },
            )]);
        let g = gated_ctx(&dir, 10_000);
        let out = llm_tool(llm_cfg(), std::sync::Arc::new(prov))
            .run_with_emit(
                &json!({"code": r#"
                    try {
                        tools.call("llm", {prompt: "hi"});
                        return { threw: false };
                    } catch (e) {
                        return { threw: true, kind: e.kind, msg: String(e) };
                    }"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["threw"], json!(true), "{}", out.content);
        assert_eq!(v["kind"], json!("llm"), "{}", out.content);
        assert!(
            v["msg"]
                .as_str()
                .unwrap_or_default()
                .contains("stream_truncated"),
            "{}",
            v
        );
        assert_eq!(
            g.budget
                .as_ref()
                .unwrap()
                .used
                .load(std::sync::atomic::Ordering::Relaxed),
            1088,
            "no usage → the reservation stands (est(\"hi\") = 1088)"
        );
        let llm = out.meta.as_ref().unwrap().get("llm").unwrap();
        assert_eq!(llm["models"]["m1"]["unknown_calls"], json!(1));
    }

    /// P1-1, max_tokens contract: the caller's cap cut the generation —
    /// kind="llm" naming the cap (NOT llm_output: the failure is
    /// truncation, not vocabulary), usage settles, no partial success.
    #[test]
    fn llm_max_tokens_is_incomplete_output_not_label_miss() {
        let dir = scratch();
        let (prov, _) =
            crate::provider::mock::MockProvider::new(vec![crate::provider::mock::MockTurn::Ok(
                crate::provider::ChatResponse {
                    blocks: vec![crate::provider::Block::Text("ye".into())],
                    stop_reason: "max_tokens".into(),
                    usage: crate::provider::mock::mk_usage(10, 8),
                },
            )]);
        let g = gated_ctx(&dir, 10_000);
        let out = llm_tool(llm_cfg(), std::sync::Arc::new(prov))
            .run_with_emit(
                &json!({"code": r#"
                    try {
                        tools.call("llm", {prompt: "hi", labels: ["yes", "no"], max_tokens: 8});
                        return { threw: false };
                    } catch (e) {
                        return { threw: true, kind: e.kind, msg: String(e) };
                    }"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["threw"], json!(true), "{}", out.content);
        assert_eq!(
            v["kind"],
            json!("llm"),
            "truncation is not a label miss: {}",
            out.content
        );
        let msg = v["msg"].as_str().unwrap_or_default();
        assert!(
            msg.contains("max_tokens") && msg.contains("ye"),
            "cap named + partial text: {msg}"
        );
        assert_eq!(
            g.budget
                .as_ref()
                .unwrap()
                .used
                .load(std::sync::atomic::Ordering::Relaxed),
            18
        );
    }

    /// P2-5, tool-side final check: a registry built with a default_model
    /// outside the allowlist refuses the omitted-model call BEFORE any
    /// reservation or provider dispatch (the setter now validates, but the
    /// registry snapshot is whatever cfg it was built with).
    #[test]
    fn llm_default_model_outside_allowlist_refused_at_call() {
        let dir = scratch();
        let (prov, state) = crate::provider::mock::MockProvider::new(vec![]);
        let cfg = CodeConfig {
            llm: crate::config::LlmConfig {
                enabled: true,
                default_model: Some("m3".into()),
                models: vec!["m1".into()],
            },
            ..CodeConfig::default()
        };
        let g = gated_ctx(&dir, 10_000);
        let out = llm_tool(cfg, std::sync::Arc::new(prov))
            .run_with_emit(
                &json!({"code": r#"
                    try {
                        tools.call("llm", {prompt: "hi"});
                        return { threw: false };
                    } catch (e) {
                        return { threw: true, kind: e.kind, msg: String(e) };
                    }"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["threw"], json!(true), "{}", out.content);
        assert_eq!(v["kind"], json!("llm"), "{}", out.content);
        let msg = v["msg"].as_str().unwrap_or_default();
        assert!(
            msg.contains("default_model") && msg.contains("allowlist"),
            "actionable message: {msg}"
        );
        assert!(
            state.captured().is_empty(),
            "no request may reach the provider"
        );
        assert_eq!(
            g.budget
                .as_ref()
                .unwrap()
                .used
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "refused before any reservation"
        );
    }

    /// External re-review P1: the termination contract is a COMPLETE-RESPONSE
    /// WHITELIST, not an abort blacklist. Providers forward unknown
    /// finish_reasons verbatim (openai maps stop/tool_calls/length only;
    /// anthropic forwards its native vocabulary), so `content_filter` /
    /// `refusal` / `pause_turn` / a future reason — even with residual text
    /// that hits a label — must come back kind="llm", never a verdict.
    /// end_turn / tool_use / stop_sequence are the three provably-complete
    /// terminations and stay successes.
    #[test]
    fn llm_stop_reason_whitelist_table() {
        let cases: &[(&str, bool)] = &[
            // Complete by construction — success.
            ("end_turn", false),
            ("tool_use", false),
            ("stop_sequence", false),
            // Abort vocabulary (agent.rs's own list).
            ("max_tokens", true),
            ("stream_error", true),
            ("stream_truncated", true),
            ("stream_idle", true),
            ("cancelled", true),
            // Filtered / refused / paused — no valid result exists.
            ("content_filter", true),
            ("refusal", true),
            ("pause_turn", true),
            // Unprovable: empty and an unknown future reason fail closed.
            ("", true),
            ("future_reason_xyz", true),
        ];
        for (sr, should_err) in cases {
            let dir = scratch();
            let (prov, state) = crate::provider::mock::MockProvider::new(vec![
                crate::provider::mock::MockTurn::Ok(crate::provider::ChatResponse {
                    // Residual text that WOULD pass the labels check — the
                    // laundering probe's exact shape.
                    blocks: vec![crate::provider::Block::Text("yes".into())],
                    stop_reason: (*sr).into(),
                    usage: crate::provider::mock::mk_usage(3, 2),
                }),
            ]);
            let g = gated_ctx(&dir, 10_000);
            let out = llm_tool(llm_cfg(), std::sync::Arc::new(prov))
                .run_with_emit(
                    &json!({"code": r#"
                        try {
                            var r = tools.call("llm", {prompt: "hi", labels: ["yes", "no"]});
                            return { threw: false, label: r.label };
                        } catch (e) {
                            return { threw: true, kind: e.kind, msg: String(e) };
                        }"#}),
                    &g,
                    &mut |_| {},
                )
                .unwrap();
            let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
            let msg = v["msg"].as_str().unwrap_or_default();
            if *should_err {
                assert_eq!(v["threw"], json!(true), "(stop_reason={sr:?}) {v}");
                assert_eq!(
                    v["kind"],
                    json!("llm"),
                    "(stop_reason={sr:?}) termination, not vocabulary: {v}"
                );
                if !sr.is_empty() {
                    assert!(msg.contains(sr), "(stop_reason={sr:?}) names it: {msg}");
                }
                if *sr == "max_tokens" {
                    assert!(
                        msg.contains("max_tokens cap"),
                        "the cap keeps its dedicated copy: {msg}"
                    );
                }
                // The call was real and billed: settle against the usage.
                assert_eq!(
                    g.budget
                        .as_ref()
                        .unwrap()
                        .used
                        .load(std::sync::atomic::Ordering::Relaxed),
                    5,
                    "(stop_reason={sr:?}) partial usage settles"
                );
            } else {
                assert_eq!(v["threw"], json!(false), "(stop_reason={sr:?}) {v}");
                assert_eq!(v["label"], json!("yes"), "(stop_reason={sr:?}) {v}");
            }
            assert_eq!(
                state.captured().len(),
                1,
                "(stop_reason={sr:?}) exactly one provider call"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// A-L2c/A-L3, unstarted arm: hooks Deny lands BEFORE dispatch — zero
    /// reservation, zero provider calls, event pair + trace + after_tool
    /// (executed=false) all still happen.
    #[test]
    fn llm_hooks_deny_runs_nothing_but_audits_everything() {
        struct DenyLlm {
            afters: std::sync::Mutex<Vec<(String, bool)>>,
        }
        impl crate::hooks::Hooks for DenyLlm {
            fn before_tool(
                &self,
                tool: &str,
                _i: &Value,
                _c: &ToolCtx,
            ) -> crate::hooks::ToolDecision {
                if tool == "llm" {
                    crate::hooks::ToolDecision::Deny("llm is off-limits".into())
                } else {
                    crate::hooks::ToolDecision::Allow
                }
            }
            fn after_tool(
                &self,
                tool: &str,
                _i: &Value,
                _o: &ToolOutput,
                executed: bool,
                _ctx: &ToolCtx,
            ) {
                self.afters
                    .lock()
                    .unwrap()
                    .push((tool.to_string(), executed));
            }
        }
        let dir = scratch();
        let (prov, state) =
            crate::provider::mock::MockProvider::new(vec![crate::provider::mock::resp_text(
                "never",
                crate::provider::mock::mk_usage(1, 1),
            )]);
        let hooks = std::sync::Arc::new(DenyLlm {
            afters: std::sync::Mutex::new(Vec::new()),
        });
        let mut g = gated_ctx(&dir, 10_000);
        g.hooks = Some(hooks.clone());
        let mut events = Vec::new();
        let out = llm_tool(llm_cfg(), std::sync::Arc::new(prov))
            .run_with_emit(
                &json!({"code": r#"
                    try {
                        tools.call("llm", {prompt: "hi"});
                        return { threw: false };
                    } catch (e) {
                        return { threw: true, kind: e.kind };
                    }"#}),
                &g,
                &mut |ev| events.push(ev),
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["threw"], json!(true), "{}", out.content);
        assert_eq!(v["kind"], json!("hooks"), "{}", out.content);
        assert_eq!(state.calls(), 0, "never dispatched");
        assert_eq!(
            g.budget
                .as_ref()
                .unwrap()
                .used
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "nothing reserved — the gate refused before the reserve"
        );
        // Event pair discipline: the announced ToolCall has its ToolResult.
        let calls = events
            .iter()
            .filter(
                |e| matches!(e, crate::agent::AgentEvent::ToolCall { name, .. } if name == "llm"),
            )
            .count();
        let results = events
            .iter()
            .filter(|e| matches!(e, crate::agent::AgentEvent::ToolResult { name, is_error: true, .. } if name == "llm"))
            .count();
        assert_eq!((calls, results), (1, 1), "event pair intact");
        assert_eq!(
            *hooks.afters.lock().unwrap(),
            vec![("llm".to_string(), false)],
            "after_tool(executed=false) fired"
        );
        // The trace records the refused call (quota honesty).
        let calls_meta = out
            .meta
            .as_ref()
            .unwrap()
            .get("calls")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(calls_meta.len(), 1);
        assert_eq!(calls_meta[0]["name"], json!("llm"));
        assert_eq!(calls_meta[0]["ok"], json!(false));
    }

    /// A-L5: an unbounded judge loop is bounded by the shared call quota —
    /// the budget latch ends it with kind="budget"; no stuck-loop
    /// interference (the guard is the outer loop's, direct runs don't trip).
    #[test]
    fn llm_judge_loop_is_bounded_by_call_quota() {
        let dir = scratch();
        let script: Vec<_> = (0..3)
            .map(|_| crate::provider::mock::resp_text("yes", crate::provider::mock::mk_usage(1, 1)))
            .collect();
        let (prov, state) = crate::provider::mock::MockProvider::new(script);
        let cfg = CodeConfig {
            max_tool_calls: 3,
            ..llm_cfg()
        };
        let out = llm_tool(cfg, std::sync::Arc::new(prov))
            .run_with_emit(
                &json!({"code": r#"
                    let n = 0;
                    try {
                        for (;;) { tools.call("llm", {prompt: "judge"}); n++; }
                    } catch (e) {
                        return { n, kind: e.kind };
                    }"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(
            v["n"],
            json!(3),
            "quota = 3 → exactly 3 run: {}",
            out.content
        );
        assert_eq!(v["kind"], json!("budget"), "{}", out.content);
        assert_eq!(state.calls(), 3, "the 4th never dispatched");
    }

    /// A-L6: the envelope vocabulary + allowlist + labels contract.
    #[test]
    fn llm_kinds_allowlist_and_labels_contract() {
        let dir = scratch();
        // Allowlist: an explicit model outside the tier is a param error —
        // kind="llm", NOTHING reserved, NOTHING dispatched.
        let (prov, state) =
            crate::provider::mock::MockProvider::new(vec![crate::provider::mock::resp_text(
                "x", None,
            )]);
        let g = gated_ctx(&dir, 10_000);
        let out = llm_tool(llm_cfg(), std::sync::Arc::new(prov))
            .run_with_emit(
                &json!({"code": r#"
                    try {
                        tools.call("llm", {prompt: "hi", model: "m3"});
                        return { threw: false };
                    } catch (e) { return { threw: true, kind: e.kind }; }"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["kind"], json!("llm"), "{}", out.content);
        assert_eq!(state.calls(), 0);
        assert_eq!(
            g.budget
                .as_ref()
                .unwrap()
                .used
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );

        // Labels: case-fold + trim + quote-wrap normalization hits the
        // CANONICAL declared spelling; a declared collision is a param error.
        let (prov, _) = crate::provider::mock::MockProvider::new(vec![
            crate::provider::mock::resp_text("  \"yes\"  ", None),
            crate::provider::mock::resp_text("maybe", None),
        ]);
        let out = llm_tool(llm_cfg(), std::sync::Arc::new(prov))
            .run_with_emit(
                &json!({"code": r#"
                    const hit = tools.call("llm", {
                        prompt: "pick", labels: ["Yes", "No", "unsure"], max_tokens: 16,
                    });
                    let miss;
                    try {
                        tools.call("llm", {prompt: "pick", labels: ["Yes", "No"], max_tokens: 16});
                        miss = "no-throw";
                    } catch (e) { miss = e.kind; }
                    let collide;
                    try {
                        tools.call("llm", {prompt: "pick", labels: ["Yes", "yes"]});
                        collide = "no-throw";
                    } catch (e) { collide = e.kind; }
                    return { label: hit.label, miss, collide };"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(
            v["label"],
            json!("Yes"),
            "canonical spelling, folded match: {}",
            out.content
        );
        assert_eq!(v["miss"], json!("llm_output"), "{}", out.content);
        assert_eq!(
            v["collide"],
            json!("llm"),
            "declared collision is a param error: {}",
            out.content
        );
    }

    /// LD-9's headline recovery split: a reserve refusal is NOT a latch —
    /// the sandbox stays alive and write_file still works afterwards.
    #[test]
    fn llm_budget_refusal_leaves_the_sandbox_alive() {
        let dir = scratch();
        let (prov, state) =
            crate::provider::mock::MockProvider::new(vec![crate::provider::mock::resp_text(
                "x", None,
            )]);
        // Cap below the estimate (est ≈ 1088) → the reserve refuses.
        let g = gated_ctx(&dir, 100);
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":["read_file","list_dir","write_file"]}"#,
        )
        .unwrap();
        let t = RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(llm_cfg())),
            &reqwest::blocking::Client::new(),
            "k",
            vec![],
            vec![],
        );
        let _ = t.provider_cell().set(std::sync::Arc::new(prov));
        let out = t
            .run_with_emit(
                &json!({"code": r#"
                    let kind, retryable, left;
                    try {
                        tools.call("llm", {prompt: "hi"});
                        kind = "none";
                    } catch (e) {
                        kind = e.kind; retryable = e.retryable;
                    }
                    const b = tools.budget();
                    left = b.tokens_left;
                    const w = tools.call("write_file", {path: "partial.txt", content: "saved"});
                    return { kind, retryable, left, wrote: typeof w };"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["kind"], json!("budget"), "{}", out.content);
        assert_eq!(v["retryable"], json!(false), "{}", out.content);
        assert_eq!(
            v["left"],
            json!(100),
            "no reservation was held: {}",
            out.content
        );
        assert_eq!(
            v["wrote"],
            json!("string"),
            "write_file still works (text result): {}",
            out.content
        );
        assert_eq!(state.calls(), 0, "never dispatched");
        // And no llm audit row — nothing ran.
        assert!(out.meta.as_ref().unwrap().get("llm").is_none());
    }

    /// A-L8 + LD-14: registry membership is a BUILD-TIME snapshot — a hot
    /// flip after the first build is not sensed, in either direction; and
    /// the gate-off contract keeps llm out of tools.list entirely.
    #[test]
    fn llm_registry_membership_is_a_build_time_snapshot() {
        let dir = scratch();
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":["read_file","list_dir"]}"#,
        )
        .unwrap();
        let shared = Arc::new(RwLock::new(CodeConfig::default()));
        let client = reqwest::blocking::Client::new();
        let tool = RunCodeTool::new(&cfg, shared.clone(), &client, "k", vec![], vec![]);
        let list = |t: &RunCodeTool| {
            let out = t
                .run_with_emit(
                    &json!({"code": r#"
                        return tools.list().some(t => t.name === "llm");"#}),
                    &ctx(&dir),
                    &mut |_| {},
                )
                .unwrap();
            out.content == "true"
        };
        assert!(!list(&tool), "A-L8: gate off → not in tools.list");
        // Hot-ON after the first build: the OnceLock registry does not
        // rebuild — the lineage keeps its snapshot (LD-14).
        *shared.write().unwrap() = llm_cfg();
        assert!(
            !list(&tool),
            "hot flip on is not sensed (registry already built without llm)"
        );
        // A fresh lineage built with the gate ON admits it…
        let on_tool = RunCodeTool::new(&cfg, shared.clone(), &client, "k", vec![], vec![]);
        assert!(list(&on_tool));
        // …and a hot-OFF after that build does not retract it either.
        *shared.write().unwrap() = CodeConfig::default();
        assert!(list(&on_tool), "hot flip off is not sensed either");
        // The description overlay follows the same def-time snapshot rule.
        let d_on = RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(llm_cfg())),
            &client,
            "k",
            vec![],
            vec![],
        )
        .def()
        .description;
        assert!(
            d_on.contains("【LLM 判别】"),
            "gate-on description carries the overlay"
        );
        let both = RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(CodeConfig {
                call_batch: true,
                ..llm_cfg()
            })),
            &client,
            "k",
            vec![],
            vec![],
        )
        .def()
        .description;
        assert!(
            both.contains("read_file / llm 等 batchable"),
            "llm on names it in the batch overlay (appendix ④ canonical): {both}"
        );
        let batch_only = RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(CodeConfig {
                call_batch: true,
                ..CodeConfig::default()
            })),
            &client,
            "k",
            vec![],
            vec![],
        )
        .def()
        .description;
        assert!(
            !batch_only.contains("read_file / llm 等"),
            "llm off keeps the closed-list contract: {batch_only}"
        );
    }

    /// LD-3: a guest-registry tool named `llm` colliding with the primitive
    /// fails the registry build (cached, actionable), not silent shadowing.
    #[test]
    fn llm_name_collision_fails_the_registry_build() {
        struct FakeLlm;
        impl Tool for FakeLlm {
            fn def(&self) -> ToolDef {
                ToolDef {
                    name: "llm".into(),
                    description: "an impostor".into(),
                    schema: json!({"type": "object"}),
                }
            }
            fn run(&self, _i: &Value, _c: &ToolCtx) -> Result<ToolOutput> {
                Ok(ToolOutput::text("impostor"))
            }
            fn available_to_code(&self) -> bool {
                true
            }
        }
        let dir = scratch();
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":["read_file","list_dir"]}"#,
        )
        .unwrap();
        let extra: Vec<Arc<Box<dyn Tool>>> = vec![Arc::new(Box::new(FakeLlm) as Box<dyn Tool>)];
        let tool = RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(llm_cfg())),
            &reqwest::blocking::Client::new(),
            "k",
            vec![],
            extra,
        );
        let out = tool
            .run_with_emit(
                &json!({"code": r#"return tools.list().length;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(out.is_error, "the registry build must fail");
        assert!(
            out.content.contains("reserves that name"),
            "actionable hint in: {}",
            out.content
        );
    }

    /// LD-9 on the batch surface: a reserve refusal inside callBatch reports
    /// kind="budget" with halted:false (machine-readable recovery split);
    /// a provider failure reports kind="llm" without retryable.
    #[test]
    fn llm_batch_entries_carry_the_recovery_vocabulary() {
        let dir = scratch();
        let (prov, _) = crate::provider::mock::MockProvider::new(vec![
            // Entry 0 (model m2, the only one the tight cap can... no — both
            // refuse; this turn serves the SECOND scenario below.
            crate::provider::mock::MockTurn::Err("boom".into()),
        ]);
        let g = gated_ctx(&dir, 100);
        let cfg = CodeConfig {
            call_batch: true,
            ..llm_cfg()
        };
        let out = llm_tool(cfg, std::sync::Arc::new(prov))
            .run_with_emit(
                &json!({"code": r#"
                    const rs = tools.callBatch([
                        { name: "llm", args: { prompt: "hi" } },
                        { name: "llm", args: { prompt: "ho" } },
                    ]);
                    return rs.map(r => r instanceof Error
                        ? { kind: r.kind, halted: r.halted, retryable: r.retryable }
                        : { ok: true });"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        for e in arr {
            assert_eq!(e["kind"], json!("budget"), "{}", out.content);
            assert_eq!(
                e["halted"],
                json!(false),
                "reserve refusal, not a latch: {}",
                out.content
            );
        }

        // Provider failure inside a batch: kind="llm", retryable omitted.
        let (prov, _) =
            crate::provider::mock::MockProvider::new(vec![crate::provider::mock::MockTurn::Err(
                "boom".into(),
            )]);
        let out = llm_tool(
            CodeConfig {
                call_batch: true,
                ..llm_cfg()
            },
            std::sync::Arc::new(prov),
        )
        .run_with_emit(
            &json!({"code": r#"
                const rs = tools.callBatch([{ name: "llm", args: { prompt: "hi" } }]);
                const r = rs[0];
                return r instanceof Error
                    ? { kind: r.kind, retryable: r.retryable }
                    : { ok: true };"#}),
            &gated_ctx(&dir, 10_000),
            &mut |_| {},
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["kind"], json!("llm"), "{}", out.content);
        assert_eq!(
            v["retryable"],
            Value::Null,
            "omitted per LD-7: {}",
            out.content
        );
    }

    /// A-L4 in-proc + A-M8 shape: callBatch × llm fan-out runs on REAL
    /// threads — a provider that records per-call [start, end] intervals
    /// lets the test assert overlap (a serialized execution cannot produce
    /// it). The e2e twin (CLI + ThreadingHTTPServer) lives in
    /// scripts/e2e_scenarios.py.
    #[test]
    fn llm_callbatch_fanout_actually_overlaps() {
        struct SlowCounting {
            spans: std::sync::Mutex<Vec<(std::time::Instant, std::time::Instant)>>,
        }
        impl crate::provider::Provider for SlowCounting {
            fn chat(
                &self,
                _req: &crate::provider::ChatRequest,
                _on_event: &mut dyn FnMut(crate::provider::ChatEvent),
            ) -> anyhow::Result<crate::provider::ChatResponse> {
                let s = std::time::Instant::now();
                std::thread::sleep(std::time::Duration::from_millis(200));
                self.spans
                    .lock()
                    .unwrap()
                    .push((s, std::time::Instant::now()));
                Ok(crate::provider::ChatResponse {
                    blocks: vec![crate::provider::Block::Text("yes".into())],
                    stop_reason: "end_turn".into(),
                    usage: crate::provider::mock::mk_usage(1, 1),
                })
            }
        }
        let dir = scratch();
        let spans = std::sync::Arc::new(SlowCounting {
            spans: std::sync::Mutex::new(Vec::new()),
        });
        let cfg = CodeConfig {
            call_batch: true,
            ..llm_cfg()
        };
        let out = llm_tool(cfg, spans.clone())
            .run_with_emit(
                &json!({"code": r#"
                    const entries = [];
                    for (let i = 0; i < 8; i++) entries.push({ name: "llm", args: { prompt: "p" } });
                    const rs = tools.callBatch(entries);
                    const labels = rs.map(r => r instanceof Error ? "err" : r.text);
                    return { n: labels.filter(t => t === "yes").length };"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["n"], json!(8), "all 8 entries succeeded: {}", out.content);
        let spans = spans.spans.lock().unwrap().clone();
        assert_eq!(spans.len(), 8);
        // At least one pair of calls overlapped in time — impossible under
        // serial dispatch; the wave engine + Parallel routing did it. (The
        // wall-clock bound is deliberately NOT asserted: in a debug build
        // the first execution of the process may still be paying wasmtime's
        // lazy module compilation, and the overlap math is the honest proof
        // regardless of clock noise.)
        let mut overlapped = 0;
        for i in 0..spans.len() {
            for j in (i + 1)..spans.len() {
                if spans[i].0 < spans[j].1 && spans[j].0 < spans[i].1 {
                    overlapped += 1;
                }
            }
        }
        assert!(
            overlapped > 0,
            "no temporal overlap — the fan-out serialized"
        );
    }

    /// A-M8 (code_memory §7, completed with this milestone per its M1/M2
    /// rows): the 判别库 verify probe — the highest-value lib shape once the
    /// llm primitive exists. `verify(n)` runs golden cases through the lib's
    /// `classify` (which rides llm with a closed label vocabulary) BEFORE any
    /// batch fan-out: all-pass returns a summary and the batch proceeds;
    /// any mismatch throws and the outer layer receives a clear error with
    /// the batch never dispatched. Both legs here: llm calls land in the
    /// per-call trace (`meta.calls`) and the LD-12 ledger, mock routed per
    /// model (M0 分流).
    const JUDGE_LIB: &str = r##"
function classify(item) {
  const r = tools.call('llm', {prompt: 'judge: ' + item, labels: ['yes', 'no'], max_tokens: 8});
  return r.label;
}
function verify(n) {
  const gold = ['yes', 'yes', 'no'];
  let pass = 0;
  for (let i = 0; i < n && i < gold.length; i++) {
    const got = classify('item-' + i);
    if (got !== gold[i]) throw new Error('verify probe failed at ' + i + ': got ' + got + ', want ' + gold[i]);
    pass++;
  }
  return 'verify ok ' + pass + '/' + n;
}
"##;

    fn am8_cfg() -> CodeConfig {
        CodeConfig {
            include: true,
            call_batch: true,
            llm: crate::config::LlmConfig {
                enabled: true,
                default_model: Some("judge".into()),
                models: vec!["judge".into()],
            },
            ..CodeConfig::default()
        }
    }

    /// [`llm_tool`] + the file tools saveLib needs (it writes the lib file
    /// and the INDEX upsert through the ordinary file tools).
    fn llm_lib_tool(
        code_cfg: CodeConfig,
        prov: std::sync::Arc<dyn crate::provider::Provider>,
    ) -> RunCodeTool {
        let t = tool_with_tools(code_cfg, r#"["read_file","write_file","list_dir"]"#);
        let _ = t.provider_cell().set(prov);
        t
    }

    #[test]
    fn am8_verify_probe_passes_then_batch_fanout_proceeds() {
        let dir = scratch();
        // Golden sequence yes/yes/no for verify(3), then the 2-entry batch.
        let (prov, state) = crate::provider::mock::MockProvider::routed(vec![(
            "judge",
            vec![
                crate::provider::mock::resp_text("yes", crate::provider::mock::mk_usage(10, 5)),
                crate::provider::mock::resp_text("yes", crate::provider::mock::mk_usage(10, 5)),
                crate::provider::mock::resp_text("no", crate::provider::mock::mk_usage(10, 5)),
                crate::provider::mock::resp_text("yes", crate::provider::mock::mk_usage(10, 5)),
                crate::provider::mock::resp_text("yes", crate::provider::mock::mk_usage(10, 5)),
            ],
        )]);
        let g = gated_ctx(&dir, 10_000);
        let code = format!(
            r#"
            saveLib('lib/judge.js', {lib:?});
            include('lib/judge.js');
            const summary = verify(3);
            const entries = [];
            for (let i = 0; i < 2; i++)
              entries.push({{name: 'llm', args: {{prompt: 'batch item ' + i, labels: ['yes', 'no'], max_tokens: 8}}}});
            const rs = tools.callBatch(entries);
            let ok = 0;
            for (const r of rs) if (!(r instanceof Error) && r.label === 'yes') ok++;
            return {{ summary: summary, ok: ok }};"#,
            lib = JUDGE_LIB
        );
        let out = llm_lib_tool(am8_cfg(), std::sync::Arc::new(prov))
            .run_with_emit(&json!({"code": code}), &g, &mut |_| {})
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["summary"], json!("verify ok 3/3"), "{}", out.content);
        assert_eq!(v["ok"], json!(2), "batch ran after a green probe: {v}");
        // 3 probe calls + 2 batch calls, ALL model-routed to "judge".
        let cap = state.captured();
        assert_eq!(cap.len(), 5);
        assert!(cap.iter().all(|c| c.model == "judge"));
        // verify's llm calls are in the trace (per-call summary) and the
        // LD-12 ledger; usage settled 5 × 15 = 75 into the shared budget.
        let meta = out.meta.as_ref().unwrap();
        let traced = meta["calls"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["name"] == json!("llm"))
            .count();
        assert_eq!(traced, 5, "probe calls are traceable: {meta}");
        let llm = &meta["llm"];
        assert_eq!(llm["calls"], json!(5));
        assert_eq!(llm["settled"], json!(75));
        assert_eq!(llm["models"]["judge"]["known_calls"], json!(5));
        assert_eq!(llm["models"]["judge"]["unknown_calls"], json!(0));
        assert_eq!(
            g.budget
                .as_ref()
                .unwrap()
                .used
                .load(std::sync::atomic::Ordering::Relaxed),
            75
        );
    }

    #[test]
    fn am8_verify_probe_failure_blocks_the_batch() {
        let dir = scratch();
        // First golden case wants "yes", the judge answers "no" — the probe
        // throws on the FIRST case; the mock has no further script (a second
        // request would fail the test via the strict-routing honest error).
        let (prov, state) = crate::provider::mock::MockProvider::routed(vec![(
            "judge",
            vec![crate::provider::mock::resp_text(
                "no",
                crate::provider::mock::mk_usage(10, 5),
            )],
        )]);
        let g = gated_ctx(&dir, 10_000);
        let code = format!(
            r#"
            saveLib('lib/judge.js', {lib:?});
            include('lib/judge.js');
            const summary = verify(3);
            const entries = [];
            for (let i = 0; i < 2; i++)
              entries.push({{name: 'llm', args: {{prompt: 'batch item ' + i}}}});
            const rs = tools.callBatch(entries);
            return {{ summary: summary, n: rs.length }};"#,
            lib = JUDGE_LIB
        );
        let out = llm_lib_tool(am8_cfg(), std::sync::Arc::new(prov))
            .run_with_emit(&json!({"code": code}), &g, &mut |_| {})
            .unwrap();
        assert!(out.is_error, "probe failure must surface as an error");
        assert!(
            out.content.contains("verify probe failed at 0"),
            "clear error to the outer layer: {}",
            out.content
        );
        // The batch NEVER dispatched: exactly one llm request reached the
        // provider (the failing probe call itself).
        assert_eq!(state.captured().len(), 1);
        // The failed exit still audits honestly (失败出口审计纪律): the one
        // call is in the trace and the ledger, its usage settled.
        let meta = out.meta.as_ref().expect("error exit still audits");
        let llm = &meta["llm"];
        assert_eq!(llm["calls"], json!(1));
        assert_eq!(llm["settled"], json!(15));
        assert_eq!(
            g.budget
                .as_ref()
                .unwrap()
                .used
                .load(std::sync::atomic::Ordering::Relaxed),
            15,
            "the probe's real cost is billed even though the run failed"
        );
    }
}
