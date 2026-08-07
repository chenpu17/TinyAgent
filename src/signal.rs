//! Wires terminal Ctrl-C into the agent's existing graceful-cancel path
//! (headless mode only). The TUI doesn't use this — in raw mode crossterm
//! delivers Ctrl-C as a key event, which the TUI handles itself.
//!
//! First SIGINT: flip a shared cancel flag the streaming reader checks between
//! chunks (≤200ms) → the run breaks → `result.json` is still written
//! (`stop_reason = cancelled`) → MCP children are cleaned via normal drop →
//! exit 130. Second SIGINT: `_exit(130)` immediately (the user insists).
//!
//! Safety: the handler only touches lock-free atomics and `_exit` — both
//! async-signal-safe. No allocation, no locks, no I/O. (Rust's
//! `std::sync::atomic` types are guaranteed lock-free wherever they compile,
//! so the handler can never accidentally call a locking `__atomic_*`.)
#[cfg(unix)]
pub(crate) fn install_sigint_cancel(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Address of the AtomicBool inside the Arc. Valid as long as any clone is
    // alive — the agent holds one for the whole run, and we move this clone in
    // (so install doesn't take ownership away from the agent's refcount).
    static CANCEL_PTR: AtomicUsize = AtomicUsize::new(0);
    // 0 = no Ctrl-C yet; 1 = graceful requested; 2+ = hard exit requested.
    static SIGINT_COUNT: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn handler(_sig: libc::c_int) {
        let n = SIGINT_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 1 {
            let raw = CANCEL_PTR.load(Ordering::SeqCst);
            if raw != 0 {
                // The pointer is valid for the run's lifetime (the agent's Arc
                // clone keeps the AtomicBool alive).
                let flag: &std::sync::atomic::AtomicBool =
                    unsafe { &*(raw as *const std::sync::atomic::AtomicBool) };
                flag.store(true, Ordering::SeqCst);
            }
        } else {
            // Second Ctrl-C: bail out now. `_exit` is async-signal-safe;
            // `std::process::exit` is not (it runs destructors / atexit).
            unsafe { libc::_exit(130) };
        }
    }

    let raw = std::sync::Arc::as_ptr(&flag) as usize;
    CANCEL_PTR.store(raw, Ordering::SeqCst);
    // Keep this Arc clone alive for the process lifetime so the stored pointer
    // stays valid even if the agent were dropped before exit (it won't be —
    // main drops the agent then exits — but this removes any doubt).
    std::mem::forget(flag);

    unsafe {
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
    }
}

/// Windows: wire console Ctrl-C into the same graceful-cancel path as unix.
/// `SetConsoleCtrlHandler` runs the handler on a dedicated thread (unlike a
/// unix signal handler, it's NOT an interrupt), so it's safe to do more here —
/// but we keep to atomics + `exit` to mirror the unix branch and stay minimal.
/// First Ctrl-C: flip the cancel flag (the run breaks within ~200ms, writes
/// result.json, exits 130). Second Ctrl-C: `exit(130)` at once.
#[cfg(windows)]
pub(crate) fn install_sigint_cancel(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CANCEL_PTR: AtomicUsize = AtomicUsize::new(0);
    // 0 = no Ctrl-C yet; 1 = graceful requested; 2+ = hard exit requested.
    static CTRL_COUNT: AtomicUsize = AtomicUsize::new(0);

    extern "system" fn handler(ctrl_type: u32) -> i32 {
        // CTRL_C_EVENT = 0, CTRL_BREAK_EVENT = 1. Anything else (close,
        // logoff, shutdown) is left to the default handler.
        if ctrl_type > 1 {
            return 0; // FALSE → not handled.
        }
        let n = CTRL_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 1 {
            let raw = CANCEL_PTR.load(Ordering::SeqCst);
            if raw != 0 {
                // Valid for the run's lifetime — the agent's Arc clone keeps
                // the AtomicBool alive (this handler's clone is forgotten).
                let flag: &std::sync::atomic::AtomicBool =
                    unsafe { &*(raw as *const std::sync::atomic::AtomicBool) };
                flag.store(true, Ordering::SeqCst);
            }
            1 // TRUE → handled: suppress the default "terminate" so the run can
              // break at its cancel check and still write result.json.
        } else {
            // Second Ctrl-C: leave now. Console-ctrl handlers run on a separate
            // thread (not an interrupt), so process::exit is acceptable here.
            std::process::exit(130);
        }
    }

    let raw = std::sync::Arc::as_ptr(&flag) as usize;
    CANCEL_PTR.store(raw, Ordering::SeqCst);
    // Keep this Arc clone alive for the process lifetime so the stored pointer
    // stays valid even if the agent is dropped before exit (mirrors unix).
    std::mem::forget(flag);

    // SetConsoleCtrlHandler lives in kernel32, linked by default on Windows.
    extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }
    unsafe {
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}

// Any other non-unix target (e.g. wasm) without a Ctrl-C analogue: no-op.
#[cfg(not(any(unix, windows)))]
pub(crate) fn install_sigint_cancel(_flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {}
