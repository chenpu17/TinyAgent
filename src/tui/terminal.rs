//! Terminal ownership: the restore guard, the tty-disconnect escape hatch and
//! the kernel diag bridge. Everything here exists so the event loop can assume
//! a raw-mode inline viewport and one owner of kernel diagnostics.

use super::worker::{Input, UiMsg};
use crossterm::event::DisableBracketedPaste;
use crossterm::terminal::disable_raw_mode;
use crossterm::ExecutableCommand;
use std::io::stdout;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Restores the terminal on drop. Installed right after raw mode is entered,
/// so a panic (or a propagated `?` error) inside the UI loop unwinds through
/// this and still leaves the user's terminal usable — without it, a panic
/// mid-draw strands them in raw mode with no echo. Inline mode lives on the
/// PRIMARY screen (no alternate screen) and captures no mouse, so teardown is
/// just bracketed-paste + raw-mode — native selection/scroll keep working.
/// Drop also clears the diag sink: the sink is process-global, so the
/// install (in `run`, right after this guard is created) and this clear stay
/// symmetric on EVERY exit path — normal quit, early `?` failure, and panic
/// unwind all drop the guard. A sink left installed after the TUI is gone
/// would keep sending into a dead channel (harmless) — worse, a later
/// non-TUI phase of the process would silently swallow diagnostics that its
/// stderr fallback was supposed to print.
pub(super) struct TerminalGuard;

fn restore_terminal() {
    let _ = stdout().execute(DisableBracketedPaste);
    let _ = disable_raw_mode();
    tiny_agent::diag::clear_diag_sink();
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// crossterm 0.29's Unix event source loops inside `poll()` when the tty
/// returns EOF: a zero-length read does not break its read loop, and non-
/// WouldBlock errors are ignored. The UI thread therefore never gets a chance
/// to observe an error after the pty master / controlling terminal disappears.
/// Probe the same tty independently so that case still has a teardown path.
#[cfg(unix)]
pub(super) fn terminal_fd_disconnected(
    fd: std::os::fd::RawFd,
    wait: Duration,
) -> std::io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = wait.as_millis().min(i32::MAX as u128) as i32;
    let rc = unsafe { libc::poll(&mut pfd, 1, millis) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0)
}

/// Matches crossterm's tty choice: stdin when it is a terminal, else `/dev/tty`.
/// The owned File keeps a separately opened descriptor alive for the monitor.
#[cfg(unix)]
struct TerminalInput {
    fd: std::os::fd::RawFd,
    _owned: Option<std::fs::File>,
}

#[cfg(unix)]
impl TerminalInput {
    fn new() -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;
        if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
            return Ok(Self {
                fd: libc::STDIN_FILENO,
                _owned: None,
            });
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")?;
        Ok(Self {
            fd: file.as_raw_fd(),
            _owned: Some(file),
        })
    }
}

/// Last-resort teardown for a terminal that no longer exists. The main thread
/// may be trapped in crossterm's internal EOF loop, so this monitor cannot just
/// send it a message. Cancel + shutdown + `None` give the worker the same stop
/// signals as a normal quit; after the same bounded wait, restore what can be
/// restored and exit the CLI rather than leaving a detached 100%-CPU process.
#[cfg(unix)]
pub(super) fn spawn_terminal_disconnect_monitor(
    in_tx: mpsc::Sender<Option<Input>>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker_exited: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::io::Result<()> {
    use std::sync::atomic::Ordering;
    let terminal = TerminalInput::new()?;
    std::thread::Builder::new()
        .name("tui-tty-watch".into())
        .spawn(move || {
            while !shutdown.load(Ordering::Acquire) {
                match terminal_fd_disconnected(terminal.fd, Duration::from_millis(100)) {
                    Ok(false) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    // An unexpected probe failure must not kill a healthy TUI.
                    Err(_) => break,
                    Ok(true) => {}
                }
                // Normal teardown may have won while the probe was blocked.
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
                cancel.store(true, Ordering::Release);
                shutdown.store(true, Ordering::Release);
                let _ = in_tx.send(None);
                let deadline = Instant::now() + Duration::from_secs(3);
                while !worker_exited.load(Ordering::Acquire) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                restore_terminal();
                std::process::exit(0);
            }
        })?;
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn spawn_terminal_disconnect_monitor(
    _in_tx: mpsc::Sender<Option<Input>>,
    _cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _worker_exited: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::io::Result<()> {
    Ok(())
}

/// Install the TUI's diag bridge: every kernel `diag_warn!` / `diag_info!`
/// (from ANY thread — the MCP readers chief among them) is packaged as a
/// [`UiMsg::Diag`] on the UI event channel instead of falling back to
/// `eprintln`, which would write from a foreign thread onto the raw-mode
/// terminal. Split out of `run` as a named function so the bridge itself is
/// testable without a terminal (see `diag_sink_forwards_kernel_diag_to_ui`).
/// Pair with [`TerminalGuard`]'s `clear_diag_sink` on teardown.
pub(super) fn install_diag_bridge(out_tx: mpsc::Sender<UiMsg>) {
    tiny_agent::diag::install_diag_sink(Box::new(move |level, msg| {
        // A failed send means the UI thread has exited; the message is
        // dropped — the alternative (falling back to eprintln) would reprint
        // the exact raw-mode garbage this bridge exists to prevent.
        let _ = out_tx.send(UiMsg::Diag(level, msg.to_owned()));
    }));
}
