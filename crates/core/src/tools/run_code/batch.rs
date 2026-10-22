//! callBatch: the `__call_batch` reserved-name dispatch, the wave engine,
//! per-entry completion tails, and the aggregate output admission.

use super::*;

/// Wave fan-out (§4): 8 per wave, aligned with the loop's parallel-batch
/// `MAX_PARALLEL_TOOL_WORKERS` so sandbox and outer fan-out share one
/// blast-radius convention.
pub(super) const BATCH_WAVE: usize = 8;
/// Shape guard on one batch (not a spec'd number): the serialized `entries`
/// array travels as ONE guest→host argument read, so the 1 MiB args cap
/// (`MAX_GUEST_READ_BYTES`, guest_api.rs) already bounds entries to ~10⁴;
/// this keeps the per-entry error-envelope build for a degenerate
/// all-refused batch cheap and the guest's result array sane. Larger work =
/// smaller batches + checkpoint files.
pub(super) const MAX_BATCH_ENTRIES: usize = 4096;
/// Aggregate char budget across ALL waves of one callBatch — the host-side
/// twin of the per-entry cap. Each entry's content is individually bounded
/// by `GUEST_OUTPUT_CHARS` (2M), but `finals` keeps every entry's clone
/// until the result array is assembled, so the old arithmetic was
/// `MAX_BATCH_ENTRIES × GUEST_OUTPUT_CHARS` ≈ 8.2G chars (~16 GB UTF-8)
/// with no knob and no fuel accounting — fuel meters wasm instructions, not
/// host clones. 8M = 4 × the single-entry cap: four full-window reads ride
/// free (a batch that stays honest never notices), while the pathological
/// case is bounded at ~tens of MB even after the serialized-array copy.
/// Past the budget, later completions degrade via [`admit_batch_output`].
pub(super) const BATCH_TOTAL_OUTPUT_CHARS: usize = 8_000_000;
// ── callBatch: read-only parallel batch (code_llm_design §4, M2) ─────────────

/// Wave-start refresh of the entry-gate latches, batch form (§4: every entry
/// runs the same 入口检查 as a single call; the wave boundary is where a
/// batch re-runs them). Mirrors `host_call_tool`'s Phase A exactly — cancel
/// and the wall clock latch `interrupt` + `halted` TOGETHER (a catchable
/// envelope must not clear the latch on retry) — but instead of failing the
/// whole call it hands back the halt state so the not-yet-started entries of
/// the wave can be refused individually (`kind`/`halted:true` entries; the
/// batch itself keeps its shape).
pub(super) fn batch_wave_halt_refresh(d: &mut HostCtx) -> Option<HaltState> {
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

/// Mid-wave execution checkpoint (external audit P2-4): unlike the
/// wave-start refresh above, this deliberately does NOT short-circuit on
/// an existing `halted` latch — a kind="budget" latch from this batch's
/// own call-limit refusal must not refuse entries that prep already
/// admitted (the quota counted them at admission; the per-entry budget
/// arithmetic in the prep loop is the sole judge of that class). It
/// refreshes ONLY the run-stopping sources — host cancel and the wall
/// clock — latching interrupt+halted together exactly like the
/// single-call entry gate, so in-flight calls complete while unstarted
/// ones refuse with the halt state and keep their event pair.
pub(super) fn batch_wave_interrupt_refresh(d: &mut HostCtx) -> Option<HaltState> {
    if d.interrupt.is_some() {
        // Already latched (cancel/deadline) — the catchable-envelope retry
        // discipline: the latch stands.
        return d.halted.clone();
    }
    if ctx_cancelled(&d.ctx) {
        d.interrupt = Some(Interrupt::Cancel);
        let h = HaltState {
            kind: "cancel",
            why: CANCEL_HALTED_WHY.to_string(),
        };
        d.halted = Some(h.clone());
        return Some(h);
    }
    if d.deadline.is_some_and(|w| Instant::now() >= w.at) {
        let why = format!(
            "run_code: {} — the sandbox is halted for the rest of this execution",
            deadline_why(&d.snap, d.deadline.map(|w| w.from_cfg).unwrap_or(false))
        );
        d.interrupt = Some(Interrupt::Deadline);
        let h = HaltState {
            kind: "deadline",
            why,
        };
        d.halted = Some(h.clone());
        return Some(h);
    }
    None
}

/// What one announced batch entry carries until it resolves (the audit /
/// hooks tail needs the ORIGINAL input and both hashes — same fields a
/// single call's Phase B/C record).
pub(super) struct BatchEntryMeta {
    pub(super) name: String,
    pub(super) args: Value,
    pub(super) in_len: usize,
    pub(super) in_hash: String,
    pub(super) in_hash_eff: Option<String>,
    pub(super) started: Instant,
}

/// The resolved outcome of one entry — `class` shapes the error envelope
/// ("tool" for a real run, the refusal classes otherwise); `halted` rides
/// only on latch-sourced refusals (LD-9: call-limit / deadline / cancel).
pub(super) struct EntryFinal {
    pub(super) output: ToolOutput,
    pub(super) class: &'static str,
    pub(super) halted: bool,
}

/// Worker → main funnel message: forwarded child events (bounded backpressure
/// channel) plus per-entry completions in true completion order.
pub(super) enum WaveMsg {
    Ev(AgentEvent),
    Done(usize, ToolOutput),
    Cancelled(usize),
}

/// The after_tool policy for one batch entry completion: `Some(executed)`
/// fires the hook with that flag, `None` skips it — reserved for the
/// deny/fs layer, which is not a hooks decision and never fires `after`
/// (the Hooks contract, mirrored from the single-call Phase B).
pub(super) type AfterFlag = Option<bool>;

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
pub(super) fn finish_batch_entry(
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
    fold_llm_audit(d, output);
}

/// Admission control of one entry outcome against the batch's aggregate
/// character budget ([`BATCH_TOTAL_OUTPUT_CHARS`]): outputs that fit keep
/// their full content and spend the budget; once it is exhausted, every
/// later completion degrades to a short placeholder. First-come-first-
/// served (completion order) by necessity — entries finish asynchronously,
/// and submission order is only known at assembly time, long after the
/// memory peak. Degradation follows the file-wide truncation discipline
/// ([`crate::tools::truncate`], `assemble_with_logs`): only `content` is
/// replaced — `is_error`, `class`, `usage` and audit `meta` stay the real
/// run's, because truncation never flips an outcome and the entry's spend
/// must still bill and audit honestly. The placeholder keeps the envelope
/// shape legal (a short string where the content was) and names the
/// recovery, mirroring how the image class says "the call ran, the result
/// just cannot be carried". `ran` picks the placeholder's CLAIM: real
/// completions say the entry ran; refusal texts — which share the budget
/// because a hooks Deny reason can be `GUEST_OUTPUT_CHARS` long — get
/// wording that does not claim execution (hooks-Deny / deadline / cancel
/// entries never started; the envelope's `kind` still carries the class).
/// Degraded placeholders do not re-spend the budget — they are bounded by
/// construction (MAX_BATCH_ENTRIES × ~200 chars ≈ 0.8 MB worst case).
pub(super) fn admit_batch_output(output: &mut ToolOutput, ran: bool, agg_left: &mut usize) {
    let n = output.content.chars().count();
    if n <= *agg_left {
        *agg_left -= n;
    } else {
        output.content = if ran {
            format!(
                "run_code: batch aggregate output cap ({BATCH_TOTAL_OUTPUT_CHARS} chars) \
                 exceeded — this entry ran but its result ({n} chars) is not carried; \
                 re-run it as a single tools.call or make earlier entries read \
                 narrower slices"
            )
        } else {
            format!(
                "run_code: batch aggregate output cap ({BATCH_TOTAL_OUTPUT_CHARS} chars) \
                 exceeded — this entry was refused before running and its refusal \
                 text is not carried; re-run it as a single tools.call for the \
                 real reason"
            )
        };
        *agg_left = 0;
    }
}

/// Refuse an announced entry and complete it through the same tail
/// ([`finish_batch_entry`]) — the entry was announced and gated, so it owes
/// the event stream its ToolResult and the audit ledger its trace whatever
/// the refusal class; only the after_tool policy varies by class. The final
/// outcome travels as the [`EntryFinal`] it becomes. Refusal texts share
/// the same aggregate budget as real outputs (a hooks Deny reason is capped
/// at `GUEST_OUTPUT_CHARS` per entry — same unbounded-total shape without
/// this), so every final passes through [`admit_batch_output`] here — with
/// `ran:false` so a degraded refusal never claims the entry ran.
pub(super) fn refuse_batch_entry(
    caller: &mut Caller<'_, HostCtx>,
    metas: &mut [Option<BatchEntryMeta>],
    finals: &mut [Option<EntryFinal>],
    idx: usize,
    mut final_: EntryFinal,
    after: AfterFlag,
    agg_left: &mut usize,
) {
    admit_batch_output(&mut final_.output, false, agg_left);
    let output = final_.output.clone();
    let meta = metas[idx].take().expect("announced entry has meta");
    finals[idx] = Some(final_);
    finish_batch_entry(caller, meta, &output, after);
}

/// One entry's D12 envelope for the result array (§4: the return shape is an
/// ARRAY of envelopes, not one envelope wrapping an array). Success carries
/// only `content`; errors carry `kind` (+ `tool`), `retryable:false` for
/// every non-"tool" class, and `halted:true` on latch-sourced refusals.
pub(super) fn batch_entry_envelope(name: Option<&str>, f: &EntryFinal) -> Value {
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
        // LD-7: "tool" AND the llm classes omit `retryable` (the model
        // judges from the message); every other class is definitive.
        if !matches!(f.class, "tool" | "llm" | "llm_output") {
            e["retryable"] = json!(false);
        }
        // LD-9's machine-readable recovery split, batch surface: budget-class
        // errors ALWAYS carry `halted` — true = a latch (call-limit /
        // deadline / cancel) refuses everything, only `return` can follow;
        // false = a reserve refusal, the sandbox is alive and settled
        // releases can re-open dispatch (write_file partial results, then
        // wrap up). Other classes carry it only when actually latched.
        if f.class == "budget" {
            e["halted"] = json!(f.halted);
        } else if f.halted {
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
pub(super) fn call_batch_dispatch(caller: &mut Caller<'_, HostCtx>, args: &Value) -> i64 {
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
    // Aggregate output budget — the host-side cap `finals` answers to. Spent
    // by real completions and refusal texts in COMPLETION order; once
    // exhausted, later outcomes degrade to short placeholders
    // ([`admit_batch_output`]). Lives outside the wave loop on purpose: the
    // peak is cross-wave (finals retains every entry until assembly).
    let mut agg_left: usize = BATCH_TOTAL_OUTPUT_CHARS;
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
                        &mut agg_left,
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
                            &mut agg_left,
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
                            &mut agg_left,
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
                                &mut agg_left,
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
                            &mut agg_left,
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
                // Pre-body latch refresh — the inline twin of the worker
                // checkpoint below, extended to the WALL CLOCK (external
                // audit P2-4): the deadline used to be checked only at wave
                // start, so a wave of slow Sequential tools kept STARTING
                // new calls long past it (8 × 160ms tools vs an 80ms
                // deadline all ran). The refresh latches exactly like the
                // single-call entry gate: in-flight calls complete, unstarted
                // entries refuse with the halt state and keep their event
                // pair.
                if let Some(h) = batch_wave_interrupt_refresh(caller.data_mut()) {
                    let output = ToolOutput::error(h.why.clone());
                    refuse_batch_entry(
                        caller,
                        &mut metas,
                        &mut finals,
                        idx,
                        EntryFinal {
                            output,
                            class: h.kind,
                            halted: true,
                        },
                        Some(false),
                        &mut agg_left,
                    );
                    continue;
                }
                // SAFETY: no workers exist yet — the emit alias is used from
                // this thread only.
                let mut emit = |ev: AgentEvent| unsafe {
                    (&mut *emit_ptr)(ev);
                };
                let mut output = run_tool(&tools, &defs, &name, &input, &ctx, &mut emit);
                let meta = metas[idx].take().expect("inline entry was announced");
                let class = real_run_class(&tools, &name, true, &output);
                // Class first (from the REAL output — an llm kind classifies
                // the run that happened), then the aggregate-budget admission
                // decides what of the content survives into finals, the event
                // pair and the audit tail — one degraded story everywhere.
                admit_batch_output(&mut output, true, &mut agg_left);
                finals[idx] = Some(EntryFinal {
                    output: output.clone(),
                    class,
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
                    // The (idx, name, effective input) of every entry that
                    // actually got a worker — entries refused at the
                    // pre-spawn latch refresh below push no handle, so the
                    // join phase must not zip against `spawn` positionally
                    // (misaligned idx↔handle would attribute panics to the
                    // wrong entry).
                    let mut launched: Vec<(usize, String, Value)> = Vec::with_capacity(spawn.len());
                    for (idx, name, input) in &spawn {
                        // Pre-spawn latch refresh on the MAIN thread
                        // (external audit P2-4's dispatch leg): the inline
                        // phase above (or earlier waves) may have spent the
                        // wall clock — do not dispatch new paid calls past
                        // the deadline. Refused entries complete here (no
                        // worker ever existed), keeping the announced event
                        // pair balanced.
                        if let Some(h) = batch_wave_interrupt_refresh(caller.data_mut()) {
                            let output = ToolOutput::error(h.why.clone());
                            refuse_batch_entry(
                                caller,
                                &mut metas,
                                &mut finals,
                                *idx,
                                EntryFinal {
                                    output,
                                    class: h.kind,
                                    halted: true,
                                },
                                Some(false),
                                &mut agg_left,
                            );
                            continue;
                        }
                        let tools = Arc::clone(&tools);
                        let defs = Arc::clone(&defs);
                        let ctx = ctx.clone();
                        let name = name.clone();
                        let input = input.clone();
                        let tx = tx.clone();
                        let idx = *idx;
                        launched.push((idx, name.clone(), input.clone()));
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
                            WaveMsg::Done(idx, mut output) => {
                                let meta = metas[idx].take().expect("worker entry was announced");
                                let class = real_run_class(&tools, &meta.name, true, &output);
                                // Same order as the inline twin: real class,
                                // then admission — see the comment there.
                                admit_batch_output(&mut output, true, &mut agg_left);
                                finals[idx] = Some(EntryFinal {
                                    output: output.clone(),
                                    class,
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
                    // Join in launch order: a panicked worker never sent Done —
                    // it gets the loop's placeholder shape (an honest error
                    // entry, after_tool(executed=true) on the dispatch that
                    // did happen). The `is_none` guard keeps a pathological
                    // panic-after-Done from overwriting (or double-finishing)
                    // the already-recorded outcome.
                    for ((idx, name, effective), h) in launched.into_iter().zip(handles) {
                        if h.join().is_err() && finals[idx].is_none() {
                            let mut output = ToolOutput::error("batch tool worker panicked");
                            // External audit P2-7: a worker that died mid-call
                            // may leave a resource hold behind (the llm
                            // primitive's LD-17 reservation, kept by
                            // ReserveGuard::Drop) — the placeholder carries
                            // the tool's re-derived audit row so the ledger
                            // explains those held tokens (model + estimate,
                            // actual:null — never fabricated).
                            if let Some(row) = tools
                                .iter()
                                .find(|t| t.def().name == name)
                                .and_then(|t| t.panic_audit_row(&effective, &ctx))
                            {
                                output.meta = Some(json!({ "llm_call": row }));
                            }
                            let meta = metas[idx].take().expect("worker entry was announced");
                            finals[idx] = Some(EntryFinal {
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

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A batchable read-only tool returning a fixed ~1.95M-char payload —
    /// just under the per-entry GUEST_OUTPUT_CHARS so each entry rides
    /// whole; shared by the aggregate-cap and envelope-fallback tests.
    fn big_out_tool() -> Arc<Box<dyn Tool>> {
        struct BigOut;
        impl Tool for BigOut {
            fn def(&self) -> ToolDef {
                ToolDef {
                    name: "big_out".into(),
                    description: "test: returns a near-cap payload".into(),
                    schema: json!({"type": "object", "properties": {}}),
                }
            }
            fn run(&self, _i: &Value, _c: &ToolCtx) -> Result<ToolOutput> {
                Ok(ToolOutput::text("x".repeat(1_950_000)))
            }
            fn available_to_code(&self) -> bool {
                true
            }
            fn batchable(&self) -> bool {
                true
            }
        }
        Arc::new(Box::new(BigOut))
    }

    /// [`batch_tool`] with the big_out payload tool registered (the cfg
    /// tools list stays minimal; extra tools land in the registry as in
    /// `batch_cancel_mid_batch_refuses_tail`).
    fn big_batch_tool(code_cfg: CodeConfig) -> RunCodeTool {
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":["read_file"],"code":{"enabled":true}}"#,
        )
        .unwrap();
        RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(code_cfg)),
            &reqwest::blocking::Client::new(),
            "k",
            vec![],
            vec![big_out_tool()],
        )
    }

    /// Host-side aggregate cap on `finals`: each entry is bounded by
    /// GUEST_OUTPUT_CHARS alone, so the retained total used to be
    /// unbounded (MAX_BATCH_ENTRIES × 2M ≈ 8.2G chars — no knob, and fuel
    /// cannot see host-side clones). A batch whose outputs together exceed
    /// BATCH_TOTAL_OUTPUT_CHARS keeps a full prefix; every later completion
    /// degrades to a SHORT placeholder that keeps the entry's outcome shape
    /// (it ran — string content stays string content, the truncation
    /// discipline of truncate()/assemble_with_logs, never a flipped
    /// is_error) and names the recovery; the run itself completes.
    #[test]
    fn batch_aggregate_output_cap_degrades_tail_entries() {
        let dir = scratch();
        // 5 × 1.95M = 9.75M chars total against an 8M budget: the first
        // four completions ride whole (7.8M spent), the fifth no longer
        // fits and degrades. Identical outputs make the test insensitive to
        // completion order — whichever four land first are the full ones.
        let out = big_batch_tool(batch_cfg())
            .run_with_emit(
                &json!({"code": r#"
                    var entries = [];
                    for (var i = 0; i < 5; i++) entries.push({name: "big_out", args: {}});
                    const rs = tools.callBatch(entries);
                    return rs.map(r => typeof r === "string"
                        ? {len: r.length, capped: r.indexOf("aggregate output cap") >= 0}
                        : {unexpected: String(r)});"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "the run itself completes: {}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        let arr = v.as_array().expect("every entry keeps its slot").clone();
        assert_eq!(arr.len(), 5, "{v}");
        let full = arr
            .iter()
            .filter(|r| r["len"] == json!(1_950_000) && r["capped"] == json!(false))
            .count();
        assert_eq!(full, 4, "the budget admits exactly four full payloads: {v}");
        let capped = arr.iter().filter(|r| r["capped"] == json!(true)).count();
        assert_eq!(capped, 1, "the over-budget tail degrades: {v}");
        assert!(
            arr.iter().any(|r| {
                r["capped"] == json!(true) && r["len"].as_u64().is_some_and(|l| l < 1000)
            }),
            "the degraded slot is a SHORT placeholder: {v}"
        );
        // The aggregate is actually capped: 4 full payloads + one
        // placeholder must stay under budget + placeholder slack.
        let total: u64 = arr.iter().map(|r| r["len"].as_u64().unwrap_or(0)).sum();
        assert!(
            total <= BATCH_TOTAL_OUTPUT_CHARS as u64 + 1_000,
            "aggregate held under the cap: {total}"
        );
        let meta = out.meta.expect("audit meta");
        assert_eq!(
            meta["call_count"], 5,
            "degraded entries still ran and audited: {meta}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F4 regression: a refusal that lands AFTER the aggregate budget is
    /// exhausted must degrade with honest copy. The old single placeholder
    /// claimed "this entry ran but its result is not carried" for refusal
    /// texts too — a hooks-Deny / deadline / cancel entry NEVER started, so
    /// the degraded refusal says refused, not ran. Staging: prep is serial
    /// per wave and the inline executions only run AFTER the wave's prep, so
    /// the wave-1 denials (entries 5-7) complete while the budget is still
    /// full — their short texts ride whole — and only the wave-2 denial
    /// (entry 8, past the 5 big_out spends) sees the exhausted budget. The
    /// degraded 5th big_out is a SUCCESS slot carrying the placeholder text
    /// (truncation never flips is_error) — the ran wording lives there.
    #[test]
    fn batch_exhausted_budget_degrades_refused_entry_honestly() {
        use crate::hooks::{Hooks, ToolDecision};

        struct DenyReads;
        impl Hooks for DenyReads {
            fn before_tool(&self, tool: &str, _i: &Value, _c: &ToolCtx) -> ToolDecision {
                if tool == "read_file" {
                    ToolDecision::Deny("no reads today".into())
                } else {
                    ToolDecision::Allow
                }
            }
        }
        let dir = scratch();
        std::fs::write(dir.join("a.txt"), "A").unwrap();
        let mut c = ctx(&dir);
        c.hooks = Some(Arc::new(DenyReads));
        let out = big_batch_tool(batch_cfg())
            .run_with_emit(
                &json!({"code": r#"
                    var entries = [];
                    for (var i = 0; i < 5; i++) entries.push({name: "big_out", args: {}});
                    for (var i = 0; i < 4; i++) entries.push({name: "read_file", args: {path: "a.txt"}});
                    const rs = tools.callBatch(entries);
                    return rs.map(r => r instanceof Error
                        ? {k: r.kind, m: String(r.message)}
                        : (typeof r === "string" && r.length > 100000 ? "ok" : String(r)));"#}),
                &c,
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "guest failed: {}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        let arr = v.as_array().expect("array result").clone();
        assert_eq!(arr.len(), 9, "{v}");
        // Wave 1: four whole payloads, the fifth degrades with the RAN
        // wording (it did run — a short success-string placeholder), and
        // the three prep-time denials keep their real reason (the budget
        // was still full at their gate).
        assert_eq!(
            arr.iter()
                .take(4)
                .filter(|r| r.as_str() == Some("ok"))
                .count(),
            4,
            "budget admits four full payloads: {v}"
        );
        assert!(
            arr[4]
                .as_str()
                .unwrap_or_default()
                .contains("this entry ran"),
            "a real run keeps the ran wording: {v}"
        );
        for r in &arr[5..8] {
            assert_eq!(r["k"], json!("hooks"), "{v}");
            assert!(
                r["m"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("no reads today"),
                "pre-exhaustion denials keep their real text: {v}"
            );
        }
        // Wave 2: the denial past the exhausted budget — honest copy, the
        // class preserved, and never a claim that the entry ran.
        assert_eq!(
            arr[8]["k"],
            json!("hooks"),
            "the refusal class survives: {v}"
        );
        let m = arr[8]["m"].as_str().unwrap_or_default();
        assert!(
            m.contains("was refused before running"),
            "a degraded refusal must say refused: {m}"
        );
        assert!(
            !m.contains("ran but") && !m.contains("no reads today"),
            "a never-run entry must not be said to have run, and the replaced text is gone: {m}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// envelope_or_trap's live failure path, end to end: the guest hogs its
    /// own heap, then asks for a batch whose result envelope (~7.8 MB) no
    /// longer fits — write_guest fails, and the fallback must deliver a
    /// SHORT same-shaped error naming the SIZE of the undeliverable result.
    /// The old fallback re-embedded the payload, re-allocating the very
    /// bytes that just failed to fit exactly when there is no room, so the
    /// retry failed too and the guest got a null pointer instead of a
    /// diagnosis. The window is deterministic: mem pinned at the 32 MiB
    /// module floor, a 26 MiB hog leaves ~6 MB free — less than the 7.8 MB
    /// envelope, orders more than the ~200 B fallback.
    #[test]
    fn undeliverable_batch_envelope_falls_back_short_with_size_only() {
        let dir = scratch();
        let out = big_batch_tool(CodeConfig {
            call_batch: true,
            mem_mb: 32, // the MODULE_INITIAL_MEMORY floor — 32 MiB, no growth
            ..CodeConfig::default()
        })
        .run_with_emit(
            &json!({"code": r#"
                var hog = "y".repeat(26 * 1024 * 1024);
                var entries = [];
                for (var i = 0; i < 5; i++) entries.push({name: "big_out", args: {}});
                var msg = "no-error";
                try { tools.callBatch(entries); }
                catch (e) { msg = String(e.message); }
                return {msg: msg, msgLen: msg.length, hogLen: hog.length};"#}),
            &ctx(&dir),
            &mut |_| {},
        )
        .unwrap();
        assert!(!out.is_error, "guest completed: {}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(
            v["hogLen"],
            json!(26 * 1024 * 1024),
            "the hog really held: {v}"
        );
        let msg = v["msg"].as_str().unwrap_or_default();
        assert!(
            msg.starts_with("run_code: host could not return the tool result (size "),
            "the fallback envelope reached the guest: {v}"
        );
        assert!(msg.contains("bytes"), "{msg}");
        assert!(
            v["msgLen"].as_u64().unwrap_or(u64::MAX) < 400,
            "short fallback, size only: {v}"
        );
        // Size ONLY: the payload (which contains the degraded entries'
        // aggregate-cap placeholders — 4 full 1.95M payloads and a marker)
        // must not leak back through the fallback.
        assert!(!msg.contains("aggregate output cap"), "{msg}");
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
        // TWO legitimate endings, both proving the flag was honored. Entry 0
        // trips the cancel flag and it STAYS set, so the epoch machinery
        // delivers the frozen hard-stop discipline ("stop at the next
        // instruction boundary") at the NEXT ticker tick (~10ms). The guest's
        // epilogue after callBatch (rs.map + return) is microseconds — but
        // under a loaded test run it can be descheduled past a tick, and the
        // whole call then ends in the run-level cancel tombstone instead of
        // returning the envelope. The envelope shape (this test's subject)
        // is asserted exactly when it does come back.
        if out.is_error {
            assert!(
                out.content.contains("[run_code] terminated: cancelled"),
                "unexpected error shape: {}",
                out.content
            );
        } else {
            let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
            assert_eq!(
                v,
                json!(["ran", {"k": "cancel", "h": true}, {"k": "cancel", "h": true}]),
                "in-flight entry completes, tail refused: {v}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// External audit P2-4, inline leg: the wall clock is refreshed before
    /// EVERY inline execution — a wave of slow Sequential tools no longer
    /// STARTS new calls long past the deadline (the audit probe: 80ms
    /// deadline, 8 × 160ms tools, all 8 ran). The in-flight entry
    /// completes; the unstarted tail refuses with kind="deadline"
    /// halted:true and keeps its event pair. Like the cancel twin above,
    /// TWO legitimate endings exist: the envelope, or the epoch trap's
    /// wall-clock tombstone (the refusal latches interrupt — the guest's
    /// epilogue races the next ~10ms tick).
    #[test]
    fn batch_deadline_refuses_unstarted_inline_entries() {
        struct SlowSeq;
        impl Tool for SlowSeq {
            fn def(&self) -> ToolDef {
                ToolDef {
                    name: "slow_seq".into(),
                    description: "test: sleeps 150ms".into(),
                    schema: json!({"type": "object", "properties": {}}),
                }
            }
            fn run(&self, _i: &Value, _c: &ToolCtx) -> Result<ToolOutput> {
                std::thread::sleep(std::time::Duration::from_millis(150));
                Ok(ToolOutput::text("done"))
            }
            fn available_to_code(&self) -> bool {
                true
            }
            fn batchable(&self) -> bool {
                true
            }
        }
        let dir = scratch();
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
            vec![Arc::new(Box::new(SlowSeq))],
        );
        // Warm the process-level module OUTSIDE the armed window — the
        // test's ctx.deadline is stamped at wall-clock time, and a cold
        // wasmtime compile (~seconds in debug) would consume it before the
        // guest starts.
        tool.run_with_emit(&json!({"code": "return 1;"}), &ctx(&dir), &mut |_| {})
            .unwrap();
        let mut c = ctx(&dir);
        c.deadline = Some(Instant::now() + std::time::Duration::from_millis(100));
        let out = tool
            .run_with_emit(
                &json!({"code": r#"
                    const entries = [1,2,3,4].map(i => ({name: "slow_seq", args: {}}));
                    const rs = tools.callBatch(entries);
                    return rs.map(r => r instanceof Error ? {k: r.kind, h: r.halted} : "ran");"#}),
                &c,
                &mut |_| {},
            )
            .unwrap();
        if out.is_error {
            assert!(
                out.content.contains("[run_code] terminated:"),
                "unexpected error shape: {}",
                out.content
            );
            assert!(
                out.content.contains("wall clock exhausted"),
                "attributed to the deadline: {}",
                out.content
            );
        } else {
            let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
            let arr = v.as_array().expect("array result");
            assert_eq!(arr.len(), 4, "{v}");
            assert_eq!(
                arr.iter().filter(|r| r.as_str() == Some("ran")).count(),
                1,
                "in-flight entry completes, unstarted tail refused: {v}"
            );
            for r in arr.iter().skip(1) {
                assert_eq!(r["k"], json!("deadline"), "{v}");
                assert_eq!(r["h"], json!(true), "{v}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P2-4, dispatch leg: after the inline phase spends the wall clock,
    /// the spawn loop refuses the Parallel (llm) entries BEFORE any worker
    /// exists — no paid inner model call starts past the deadline.
    #[test]
    fn batch_deadline_after_inline_blocks_llm_dispatch() {
        struct SlowSeq;
        impl Tool for SlowSeq {
            fn def(&self) -> ToolDef {
                ToolDef {
                    name: "slow_seq".into(),
                    description: "test: sleeps 150ms".into(),
                    schema: json!({"type": "object", "properties": {}}),
                }
            }
            fn run(&self, _i: &Value, _c: &ToolCtx) -> Result<ToolOutput> {
                std::thread::sleep(std::time::Duration::from_millis(150));
                Ok(ToolOutput::text("done"))
            }
            fn available_to_code(&self) -> bool {
                true
            }
            fn batchable(&self) -> bool {
                true
            }
        }
        let dir = scratch();
        // Empty script: any llm request that slipped past the checkpoint
        // fails the mock loudly — captured() must stay empty.
        let (prov, state) = crate::provider::mock::MockProvider::new(vec![]);
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":["read_file"],"code":{"enabled":true}}"#,
        )
        .unwrap();
        let tool = RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(CodeConfig {
                call_batch: true,
                ..llm_cfg()
            })),
            &reqwest::blocking::Client::new(),
            "k",
            vec![],
            vec![Arc::new(Box::new(SlowSeq))],
        );
        let _ = tool.provider_cell().set(std::sync::Arc::new(prov));
        tool.run_with_emit(&json!({"code": "return 1;"}), &ctx(&dir), &mut |_| {})
            .unwrap();
        let mut c = ctx(&dir);
        c.deadline = Some(Instant::now() + std::time::Duration::from_millis(100));
        let out = tool
            .run_with_emit(
                &json!({"code": r#"
                    const rs = tools.callBatch([
                        {name: "slow_seq", args: {}},
                        {name: "slow_seq", args: {}},
                        {name: "llm", args: {prompt: "p1"}},
                        {name: "llm", args: {prompt: "p2"}}
                    ]);
                    return rs.map(r => r instanceof Error ? {k: r.kind, h: r.halted} : "ran");"#}),
                &c,
                &mut |_| {},
            )
            .unwrap();
        // The headline assertion holds in BOTH legitimate endings (envelope
        // or the wall-clock tombstone): no llm dispatch ever happened.
        assert!(
            state.captured().is_empty(),
            "no inner model call may start past the deadline"
        );
        if !out.is_error {
            let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
            let arr = v.as_array().expect("array result");
            assert_eq!(arr.len(), 4, "{v}");
            assert_eq!(arr[0], json!("ran"), "in-flight entry completes: {v}");
            for r in arr.iter().skip(1) {
                assert_eq!(r["k"], json!("deadline"), "{v}");
                assert_eq!(r["h"], json!(true), "{v}");
            }
        } else {
            assert!(
                out.content.contains("wall clock exhausted"),
                "unexpected error shape: {}",
                out.content
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Coverage gap (external review round 3): the WAVE-START halt refresh
    /// on the multi-wave boundary. Wave 1 fully launches (parallel tool,
    /// cancel lands mid-flight from inside one worker); the wave-2 start
    /// must then refuse every not-yet-started entry with
    /// {kind:"cancel", halted:true} — event pairing intact, wave-1 entries
    /// all complete.
    #[test]
    fn batch_wave_start_halt_refuses_second_wave() {
        struct CancellingPar;
        impl Tool for CancellingPar {
            fn def(&self) -> ToolDef {
                ToolDef {
                    name: "cancelling_par".into(),
                    description: "test: sleeps, then sets cancel".into(),
                    schema: json!({"type": "object", "properties": {}}),
                }
            }
            fn run(&self, _i: &Value, c: &ToolCtx) -> Result<ToolOutput> {
                std::thread::sleep(std::time::Duration::from_millis(120));
                if let Some(f) = &c.cancel {
                    f.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(ToolOutput::text("done"))
            }
            fn available_to_code(&self) -> bool {
                true
            }
            fn batchable(&self) -> bool {
                true
            }
            fn concurrency(&self) -> crate::tools::ToolConcurrency {
                crate::tools::ToolConcurrency::Parallel
            }
        }
        let dir = scratch();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
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
            vec![Arc::new(Box::new(CancellingPar))],
        );
        // Warm the module outside the guest run (debug-build compile time).
        tool.run_with_emit(&json!({"code": "return 1;"}), &ctx(&dir), &mut |_| {})
            .unwrap();
        let mut c = ctx(&dir);
        c.cancel = Some(Arc::clone(&cancel));
        let out = tool
            .run_with_emit(
                &json!({"code": r#"
                    const entries = [];
                    for (let i = 0; i < 12; i++) entries.push({name: "cancelling_par", args: {}});
                    const rs = tools.callBatch(entries);
                    return rs.map(r => r instanceof Error ? {k: r.kind, h: r.halted} : "ran");"#}),
                &c,
                &mut |_| {},
            )
            .unwrap();
        // The guest's tail calls hit the entry-gate latch after the halt —
        // both legitimate endings carry the refusal shape or the tombstone.
        if out.is_error {
            assert!(
                out.content.contains("[run_code] terminated:"),
                "unexpected error shape: {}",
                out.content
            );
        } else {
            let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
            let arr = v.as_array().expect("array result");
            assert_eq!(arr.len(), 12, "{v}");
            assert_eq!(
                arr.iter().filter(|r| r.as_str() == Some("ran")).count(),
                8,
                "wave 1 (8 parallel) fully completes: {v}"
            );
            for r in arr.iter().skip(8) {
                assert_eq!(r["k"], json!("cancel"), "wave-2 refused at wave start: {v}");
                assert_eq!(r["h"], json!(true), "{v}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Coverage gap (external review round 3): the deny/fs branches INSIDE a
    /// batch. A hooks Deny fires `after_tool(executed=false)` (the Hooks
    /// contract reserves it for before_tool Deny); the deny/fs policy layer
    /// does NOT fire after_tool at all. Both legs land as per-entry Error
    /// envelopes with their own kinds — the batch keeps its shape.
    #[test]
    fn batch_hooks_deny_and_fs_deny_entry_shapes() {
        struct Recorder;
        impl crate::hooks::Hooks for Recorder {
            fn before_tool(
                &self,
                tool: &str,
                input: &Value,
                _c: &ToolCtx,
            ) -> crate::hooks::ToolDecision {
                if tool == "read_file"
                    && input
                        .get("path")
                        .and_then(Value::as_str)
                        .is_some_and(|p| p.ends_with("secret.js"))
                {
                    crate::hooks::ToolDecision::Deny("no secrets in batch".into())
                } else {
                    crate::hooks::ToolDecision::Allow
                }
            }
            fn after_tool(
                &self,
                tool: &str,
                _input: &Value,
                _output: &ToolOutput,
                executed: bool,
                _c: &ToolCtx,
            ) {
                AFTER_SEEN
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((tool.to_string(), executed));
            }
        }
        AFTER_SEEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        let dir = scratch();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/a.js"), "globalThis.a = 1;").unwrap();
        AFTER_SEEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        let mut g = ctx(&dir);
        g.hooks = Some(Arc::new(Recorder));
        let tool = tool_with_tools(
            CodeConfig {
                call_batch: true,
                deny_paths: vec!["lib/blocked.js".into()],
                ..include_cfg()
            },
            r#"["read_file","write_file","list_dir"]"#,
        );
        let out = tool
            .run_with_emit(
                &json!({"code": r#"
                    const rs = tools.callBatch([
                        {name: "read_file", args: {path: "lib/a.js"}},
                        {name: "read_file", args: {path: "lib/secret.js"}},
                        {name: "read_file", args: {path: "lib/blocked.js"}}
                    ]);
                    return rs.map(r => r instanceof Error
                        ? {k: r.kind, m: String(r).slice(0, 60)} : "ran");"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "guest failed: {}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        let arr = v.as_array().expect("array result");
        assert_eq!(arr.len(), 3, "{v}");
        assert_eq!(arr[0], json!("ran"), "clean entry unaffected: {v}");
        assert_eq!(arr[1]["k"], json!("hooks"), "hooks Deny kind: {v}");
        assert!(
            arr[1]["m"]
                .as_str()
                .unwrap_or_default()
                .contains("no secrets in batch"),
            "{}",
            v
        );
        assert_eq!(arr[2]["k"], json!("denied"), "deny/fs layer kind: {v}");
        let seen = AFTER_SEEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let ran = seen.iter().filter(|(t, e)| t == "read_file" && *e).count();
        assert_eq!(
            ran, 1,
            "exactly the clean entry reports after_tool(executed=true): {seen:?}"
        );
        assert!(
            seen.contains(&("read_file".to_string(), false)),
            "hooks Deny is the ONE denied class that still fires after_tool(executed=false): {seen:?}"
        );
        // The deny/fs leg never executed the tool and never passed the
        // hooks gate — exactly TWO after_tool rows total (ran + hooks-deny).
        assert_eq!(
            seen.len(),
            2,
            "the deny/fs layer does NOT fire after_tool (its executed=false is reserved for before_tool Deny): {seen:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Coverage gap (external review round 3): the batch shape guard fires
    /// in the serial prep phase — a 4097-entry batch is refused whole, zero
    /// entries dispatched.
    #[test]
    fn batch_over_entry_cap_refused_whole() {
        let dir = scratch();
        let v = js_out(
            &lib_tool_batch(),
            &dir,
            r#"
            const entries = [];
            for (let i = 0; i < 4097; i++) entries.push({name: "read_file", args: {path: "nope" + i}});
            try { tools.callBatch(entries); return { threw: false }; }
            catch (e) { return { threw: true, k: e.kind, m: String(e) } }"#,
        );
        assert_eq!(v["threw"], json!(true), "{}", v);
        let msg = v["m"].as_str().unwrap_or_default();
        assert!(msg.contains("4096"), "the cap is named: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Coverage gap (external review round 3): an entry whose tool returns
    /// an IMAGE gets the per-entry "image" error envelope (run_code cannot
    /// carry images into the sandbox) — the batch keeps its shape and the
    /// other entries are unaffected.
    #[test]
    fn batch_image_entry_gets_image_envelope() {
        struct ImageTool;
        impl Tool for ImageTool {
            fn def(&self) -> ToolDef {
                ToolDef {
                    name: "pic_out".into(),
                    description: "test: returns an image".into(),
                    schema: json!({"type": "object", "properties": {}}),
                }
            }
            fn run(&self, _i: &Value, _c: &ToolCtx) -> Result<ToolOutput> {
                Ok(ToolOutput {
                    content: "a picture".into(),
                    image: Some(crate::provider::Block::Image {
                        media_type: "image/png".into(),
                        data: "bm90LXJlYWxseS1wbmc".into(),
                    }),
                    is_error: false,
                    usage: None,
                    usage_in_shared_budget: false,
                    meta: None,
                })
            }
            fn available_to_code(&self) -> bool {
                true
            }
            fn batchable(&self) -> bool {
                true
            }
        }
        let dir = scratch();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/a.js"), "globalThis.a = 1;").unwrap();
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
            vec![Arc::new(Box::new(ImageTool))],
        );
        let out = tool
            .run_with_emit(
                &json!({"code": r#"
                    const rs = tools.callBatch([
                        {name: "read_file", args: {path: "lib/a.js"}},
                        {name: "pic_out", args: {}}
                    ]);
                    return rs.map(r => r instanceof Error ? {k: r.kind} : "ran");"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        let arr = v.as_array().expect("array result");
        assert_eq!(arr.len(), 2, "{v}");
        assert_eq!(arr[0], json!("ran"), "{}", v);
        assert_eq!(arr[1]["k"], json!("image"), "image entry envelope: {v}");
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// the entry completes as the panic placeholder, and the placeholder
    /// now carries the llm audit row (model + re-derived estimate,
    /// actual:null) so meta.llm explains the reservation the guard's Drop
    /// kept. Unknown usage stays unknown — never fabricated.
    #[test]
    fn batch_llm_panic_placeholder_keeps_audit() {
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
        let out = llm_tool(
            CodeConfig {
                call_batch: true,
                ..llm_cfg()
            },
            std::sync::Arc::new(PanicProvider),
        )
        .run_with_emit(
            &json!({"code": r#"
                const rs = tools.callBatch([{ name: "llm", args: { prompt: "hi" } }]);
                return rs.map(r => r instanceof Error ? String(r) : "ok");"#}),
            &g,
            &mut |_| {},
        )
        .unwrap();
        assert!(
            !out.is_error,
            "the batch completes; the entry carries the panic: {}",
            out.content
        );
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert!(
            v.as_array()
                .and_then(|a| a.first())
                .and_then(|e| e.as_str())
                .unwrap_or_default()
                .contains("batch tool worker panicked"),
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
            "ReserveGuard::Drop kept est(\"hi\") = 1088 through the worker unwind"
        );
        let llm = out.meta.as_ref().unwrap().get("llm").unwrap();
        assert_eq!(llm["calls"], json!(1), "the placeholder folds an audit row");
        assert_eq!(llm["reserved"], json!(1088), "the row explains the hold");
        assert_eq!(llm["models"]["m1"]["unknown_calls"], json!(1));
        assert_eq!(llm["models"]["m1"]["known_calls"], json!(0));
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
