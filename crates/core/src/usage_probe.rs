//! Per-worker usage side channel for the parallel tool batch.
//!
//! A `Parallel` tool (the `task` sub-agent, the sandbox `llm` primitive) bills
//! each inner chat into the run-wide `BudgetGate` as it runs. If its worker
//! thread PANICS before returning, the `ToolOutput` carrying that usage dies
//! with the thread: the gate counter has the spend, but `RunOutcome` /
//! `result.json` totals never see it (the join arm can only synthesize a
//! usage-less error). This is the external-review "known divergence" — under
//! `panic = "abort"` (release) the process dies instead, so the gap only
//! exists in unwind builds.
//!
//! The channel: the parent installs a fresh probe on the worker THREAD before
//! running the tool; `Agent::bill_chat` (and the sandbox `llm` primitive)
//! records each billed `Usage` into the thread-local probe. The parent keeps
//! the `Arc`, so after `join()` returns Err it can read the usage that
//! survived the unwind and attach it to the synthesized error output. Normal
//! (non-panicking) workers ignore the probe entirely — the `ToolOutput` is the
//! authority there.
//!
//! Scoping by THREAD, not by `ToolCtx`, is deliberate: the probe is an
//! implementation detail of the parallel join arm, not a capability tools
//! should read, and it automatically follows a `task` child's nested chats
//! (they run on the same worker thread). `thread::scope` spawns a fresh OS
//! thread per worker, so probes never leak between calls.
//!
//! Known edge (honest limit): a `task` child that runs its OWN parallel batch
//! and then panics AFTER that nested batch joined successfully contributes
//! only its direct chats to this probe — the nested children's usage already
//! folded into the child's totals but was not re-recorded here. Re-recording
//! folds would double-count the `llm` primitive (which records directly), and
//! the two sources are indistinguishable at the fold point, so this rare
//! nested-panic case may still under-report totals relative to the counter.

use crate::provider::Usage;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

thread_local! {
    /// Set once per parallel worker thread. `None` on the main agent thread
    /// and on any thread not spawned by the parallel batch — recording there
    /// is a no-op.
    static PROBE: RefCell<Option<Arc<Mutex<Usage>>>> = const { RefCell::new(None) };
}

/// Install `probe` as the current thread's usage sink. Called by the worker
/// closure before it runs the tool body; later calls overwrite (a nested
/// batch would intentionally redirect to its own probe).
pub(crate) fn install(probe: Arc<Mutex<Usage>>) {
    PROBE.with(|slot| *slot.borrow_mut() = Some(probe));
}

/// Add one billed `Usage` to the current thread's probe, if any. Saturating —
/// the same anti-wrap discipline as every other usage accumulator.
pub(crate) fn record(u: &Usage) {
    if u.total() == 0 {
        return;
    }
    PROBE.with(|slot| {
        if let Some(probe) = slot.borrow().as_ref() {
            let mut acc = probe.lock().unwrap_or_else(|e| e.into_inner());
            acc.input_tokens = acc.input_tokens.saturating_add(u.input_tokens);
            acc.output_tokens = acc.output_tokens.saturating_add(u.output_tokens);
            acc.cache_creation_input_tokens = acc
                .cache_creation_input_tokens
                .saturating_add(u.cache_creation_input_tokens);
            acc.cache_read_input_tokens = acc
                .cache_read_input_tokens
                .saturating_add(u.cache_read_input_tokens);
        }
    });
}

/// Read the probe's accumulated usage (the parent-side join arm calls this
/// only after `join()` failed).
pub(crate) fn read(probe: &Mutex<Usage>) -> Usage {
    *probe.lock().unwrap_or_else(|e| e.into_inner())
}
