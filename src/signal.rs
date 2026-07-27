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

#[cfg(not(unix))]
pub(crate) fn install_sigint_cancel(_flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    // Windows console Ctrl-C arrives as a CTRL_C_EVENT, not SIGINT, and isn't
    // wired here. Headless Ctrl-C on Windows = default hard exit (documented
    // in README). Wiring it needs SetConsoleCtrlHandler — out of scope.
}
