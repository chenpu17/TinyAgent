//! Diagnostics channel for library-internal warnings and one-off notes.
//!
//! The kernel is embedded not only by the CLI (which owns a console) but by
//! GUI hosts and language bindings, where anything written straight to the
//! process stderr is invisible: no terminal is attached and the embedder has
//! no way to capture Rust's `eprintln!`. Every soft failure that used to be a
//! bare `eprintln!` in library code paths (skill-scan warnings, MCP connect
//! degradation, session-corruption notes, run_code's malformed-marker
//! fallback) goes through [`diag_warn!`] / [`diag_info!`] instead:
//!
//! * default (no sink installed) the message is printed to stderr verbatim —
//!   byte-identical to the pre-channel behavior the CLI relies on;
//! * a host that calls [`install_diag_sink`] receives `(level, message)` as
//!   data and can route it to a log pane, a file, or its own facade.
//!
//! Why a process-global sink rather than per-agent plumbing: the noisiest
//! sites fire at ASSEMBLY time (before any run exists, so no event stream is
//! attached yet) and inside background threads (the MCP readers) that hold no
//! `ToolCtx`. Threading a sink parameter through all of those would touch
//! every signature for one optional feature; a global with replace semantics
//! keeps the call sites one-word diffs. `RwLock<Option<..>>` (not a
//! `OnceLock`) so tests can install, assert, and restore the fallback.

use std::sync::{Arc, RwLock};

/// Coarse severity. Most channel traffic is soft-failure warnings; [`Info`]
/// marks progress notes the CLI prints as-is (e.g. `mcp: server ready`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagLevel {
    /// Progress note, printed as-is (e.g. `mcp: server ready`).
    Info,
    /// Soft failure the host should surface (a skipped skill, a disabled
    /// compression, …) — usually prefixed with a bracketed tag in the text.
    Warn,
}

/// The sink as it crosses the install boundary — boxed, the historical
/// shape (kept so hosts' `install_diag_sink(Box::new(..))` calls compile
/// unchanged); [`install_diag_sink`] moves it into the shared [`Arc`]
/// storage right away.
type Sink = Box<dyn Fn(DiagLevel, &str) + Send + Sync>;

/// The installed sink, behind an [`Arc`] so [`emit`] can clone it out of the
/// channel's lock and drop the lock BEFORE calling it (re-entrancy safety —
/// see [`emit`] and [`install_diag_sink`]).
type SharedSink = Arc<dyn Fn(DiagLevel, &str) + Send + Sync>;

static SINK: RwLock<Option<SharedSink>> = RwLock::new(None);

/// Install (replace) the process-global diagnostics sink. The boxed sink is
/// moved into the shared [`Arc`] storage, so replacing it later never tears
/// a call in flight. It is called on every subsequent `diag::diag_warn!` /
/// `diag::diag_info!` from any thread; [`clear_diag_sink`] restores the
/// stderr fallback.
///
/// Re-entrancy contract: the sink MAY itself call `diag_warn!` /
/// `diag_info!` (directly or through a library it uses) — the channel's lock
/// is never held during the call, so the nested emission simply walks the
/// full [`emit`] path again. A sink that re-enters UNCONDITIONALLY recurses
/// without bound; bounding its own re-entry is the sink's responsibility.
pub fn install_diag_sink(sink: Sink) {
    *SINK.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::from(sink));
}

/// Restore the default stderr fallback (what the CLI runs with).
pub fn clear_diag_sink() {
    *SINK.write().unwrap_or_else(|e| e.into_inner()) = None;
}

pub(crate) fn emit(level: DiagLevel, msg: &str) {
    // Clone the Arc out and DROP the read guard before calling the sink. A
    // sink that re-enters the channel would otherwise acquire SINK.read()
    // twice on the same thread, and std's RwLock documents that a recursive
    // read acquisition may deadlock (a writer queuing between the two
    // acquisitions blocks the second read). With the guard released, the
    // re-entrant call takes the unguarded full path above again.
    let sink = SINK.read().unwrap_or_else(|e| e.into_inner()).clone();
    match sink {
        Some(f) => f(level, msg),
        None => eprintln!("{msg}"),
    }
}

/// Tests that touch the global sink serialize on this lock — two tests
/// installing/clearing concurrently would steal each other's captures.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

macro_rules! diag_warn {
    ($($arg:tt)*) => {
        $crate::diag::emit($crate::diag::DiagLevel::Warn, &format!($($arg)*))
    };
}
pub(crate) use diag_warn;

macro_rules! diag_info {
    ($($arg:tt)*) => {
        $crate::diag::emit($crate::diag::DiagLevel::Info, &format!($($arg)*))
    };
}
pub(crate) use diag_info;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Install → capture → clear → fallback path doesn't panic. Asserts
    /// CONTAINS, not index 0: the sink is process-global and anything else
    /// serialized on TEST_LOCK is absent by construction, but stays robust.
    #[test]
    fn sink_captures_and_clear_restores_fallback() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let got: std::sync::Arc<Mutex<Vec<(DiagLevel, String)>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        install_diag_sink(Box::new(move |lvl, msg| {
            g.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((lvl, msg.to_owned()))
        }));
        diag_warn!("[diag-test] warn payload {}", 42);
        diag_info!("[diag-test] info payload");
        clear_diag_sink();
        // Post-clear emission takes the eprintln fallback — it must reach
        // neither the cleared sink (assert below) nor panic on the way out.
        diag_warn!("[diag-test] fallback goes to stderr");
        let got = got.lock().unwrap_or_else(|e| e.into_inner());
        assert!(got.contains(&(DiagLevel::Warn, "[diag-test] warn payload 42".to_owned())));
        assert!(got.contains(&(DiagLevel::Info, "[diag-test] info payload".to_owned())));
        assert!(!got.iter().any(|(_, m)| m.contains("fallback")));
    }

    /// A sink that re-enters the channel (a host whose own logging stack —
    /// or a library it calls — hits `diag_warn!` again) must neither
    /// deadlock nor lose the nested message: `emit` releases the read guard
    /// BEFORE calling the sink, so the nested emission re-enters the full
    /// path with a free lock. The one-shot flag stands in for the host-side
    /// recursion bound the contract puts in the sink's own hands (a
    /// deadlock here manifests as this test hanging, not failing).
    #[test]
    fn reentrant_sink_completes_nested_emissions_without_deadlock() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let got: std::sync::Arc<Mutex<Vec<(DiagLevel, String)>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let reentered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (g, once) = (got.clone(), reentered.clone());
        install_diag_sink(Box::new(move |lvl, msg| {
            g.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((lvl, msg.to_owned()));
            if !once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                diag_warn!("[diag-test] nested warn");
            }
        }));
        diag_info!("[diag-test] outer info");
        clear_diag_sink();
        let got = got.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            got.contains(&(DiagLevel::Info, "[diag-test] outer info".to_owned())),
            "outer emission must land: {got:?}"
        );
        assert!(
            got.contains(&(DiagLevel::Warn, "[diag-test] nested warn".to_owned())),
            "re-entrant emission must land via the full emit path: {got:?}"
        );
    }
}
