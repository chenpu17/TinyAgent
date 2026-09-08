use super::{truncate, Tool, ToolCtx, ToolOutput};
use crate::provider::ToolDef;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::process::Command;
use std::time::{Duration, Instant};

pub struct Shell;

impl Tool for Shell {
    /// Guest-registry opt-in (run_code's `tools.call` surface) — shell is
    /// part of the sandbox's built-in I/O substrate (byte pipelines, file
    /// ops the JS stdlib lacks).
    fn available_to_code(&self) -> bool {
        true
    }

    fn def(&self) -> ToolDef {
        ToolDef {
            name: "shell".into(),
            description: "Run a shell command in the working directory and return \
                          stdout+stderr (combined; truncated to the output cap). Use for \
                          builds, tests, git, and CLI work - and for DATA: prefer computing \
                          over reading large files into the conversation (awk/sort/uniq \
                          pipelines, or a heredoc script: `python3 - <<'PY' ... PY`). \
                          Redirect bulky output to a file and grep/read it. A background \
                          job that keeps the output pipes open blocks completion - redirect \
                          its output (`cmd >/dev/null 2>&1 &`) to let it survive across \
                          calls. Each command is killed after shell_timeout_secs \
                          (default 120s); split longer work."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run"}
                },
                "required": ["command"]
            }),
        }
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let cmd = input["command"]
            .as_str()
            .context("shell: missing \"command\" string")?;

        // Spawn the child in its own process group (unix) so that a timeout
        // can kill the whole tree, not just the direct `sh`/`cmd` process —
        // a single `sh -c make` execs make, but make spawns compilers; those
        // grandchildren share the child's group and would otherwise survive a
        // plain `child.kill()` and keep the build running in the background.
        // The same group kill also reclaims background processes left holding
        // the output pipes after a normal exit (collect_output below).
        let mut child = {
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                Command::new("sh")
                    .args(["-c", cmd])
                    .current_dir(&ctx.workdir)
                    // stdin null: a command that reads stdin would otherwise
                    // inherit OUR stdin and hang forever on a prompt (or eat
                    // the harness's piped input in batch mode).
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .process_group(0)
                    .spawn()
            }
            #[cfg(not(unix))]
            {
                Command::new("cmd")
                    .args(["/C", cmd])
                    .current_dir(&ctx.workdir)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
            }
        }
        .with_context(|| format!("failed to spawn shell for: {cmd}"))?;

        // Drain stdout/stderr on reader threads, capped in memory: beyond the
        // cap the output is discarded but still drained, so the child never
        // blocks on a full pipe.
        //
        // This cap is in BYTES, but truncate() below counts CHARS. Size it at
        // 4 bytes/char (the max UTF-8 width) + slack so that for ANY content —
        // including CJK/emoji, where 3-4 bytes collapse to one char — enough
        // bytes survive for the char-based truncate() to hit its threshold and
        // append the "[truncated]" marker. A naive byte cap of max_output_chars
        // left multi-byte output silently short of the char limit, so truncate()
        // never fired and the model got truncated output with no sign it was cut.
        let cap = ctx.max_output_chars.saturating_mul(4).saturating_add(1024);
        let mut out_pipe = child.stdout.take().expect("stdout piped");
        let mut err_pipe = child.stderr.take().expect("stderr piped");
        // Each reader SENDS its bytes at EOF (instead of returning them) so
        // the parent can wait with a deadline — an unconditional join here is
        // the pre-fix bug: a leftover background process holding the pipe
        // kept EOF from ever arriving and hung the turn forever (see
        // collect_output).
        let (tx, rx) = std::sync::mpsc::channel::<ReaderDone>();
        let tx_err = tx.clone();
        let out_thread = std::thread::spawn(move || {
            let _ = tx.send(ReaderDone::Stdout(read_capped(&mut out_pipe, cap)));
        });
        let err_thread = std::thread::spawn(move || {
            let _ = tx_err.send(ReaderDone::Stderr(read_capped(&mut err_pipe, cap)));
        });

        // Poll the child so a timeout or cancel can kill it (wait_with_output
        // would consume the handle and leak the process on timeout).
        // `0` = no per-command limit: cancel still applies (poll below), and
        // the run-level timeout_secs remains the backstop. checked_add: a
        // huge shell_timeout_secs can overflow the clock on some platforms
        // (a plain `Instant + Duration` panics there); such a value is
        // "no limit" for all practical purposes, so None is the honest
        // rendering, identical to 0.
        let deadline = (ctx.shell_timeout_secs > 0)
            .then(|| Instant::now().checked_add(Duration::from_secs(ctx.shell_timeout_secs)))
            .flatten();
        let mut cancelled = false;
        let status = loop {
            // Check exit FIRST (C10): a command that finished within the same
            // 50ms poll slice as a cancel must report its REAL exit code and
            // output — the cancel aimed at a command that already exited
            // would otherwise mislabel a successful run as cancelled. Only a
            // still-running child is cancellable.
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) => {
                    // Honor an interactive cancel (Ctrl-C in the TUI) between
                    // polls, killing the whole process group the same way a
                    // timeout does.
                    if let Some(c) = &ctx.cancel {
                        if c.load(std::sync::atomic::Ordering::Relaxed) {
                            cancelled = true;
                            kill_process_group(&mut child);
                            let _ = child.wait();
                            break None;
                        }
                    }
                    if deadline.is_some_and(|d| Instant::now() >= d) {
                        kill_process_group(&mut child);
                        let _ = child.wait();
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    kill_process_group(&mut child);
                    let _ = child.wait();
                    return Err(e).context("shell: failed to wait on command");
                }
            }
        };

        // Collect the readers, reclaiming leftover background processes that
        // hold the pipes open past the child's exit (collect_output): the
        // pre-fix unconditional join could block FOREVER on this path.
        // `killed` = the poll loop already killed the process group (timeout
        // / cancel); collect_output must not killpg AGAIN on an already-reaped
        // pid (pid reuse could hit an unrelated new process group).
        let killed = status.is_none();
        let c = collect_output(&rx, &mut child, ctx, killed);
        // Join only completed readers — a leaked one is still blocked in
        // read(); dropping its handle detaches it (it exits at process exit).
        if !c.leaked {
            let _ = out_thread.join();
            let _ = err_thread.join();
        }
        let stdout = c.stdout;
        let stderr = c.stderr;

        let Some(status) = status else {
            let mut out = format!("$ {cmd}\n");
            out.push_str(&decode_output(&stdout));
            // Keep stderr on the killed path too: the diagnostics a command
            // printed before hanging are exactly what's needed to see WHY it
            // timed out — dropping them makes timeouts undebuggable.
            let stderr = decode_output(&stderr);
            if !stderr.trim().is_empty() {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("[stderr]\n");
                out.push_str(&stderr);
            }
            truncate(&mut out, ctx.max_output_chars);
            if cancelled {
                out.push_str("\n[command cancelled by user]");
            } else {
                out.push_str(&format!(
                    "\n[command timed out after {}s and was killed]",
                    ctx.shell_timeout_secs
                ));
            }
            return Ok(ToolOutput::error(out));
        };

        let mut out = String::new();
        out.push_str(&format!("$ {cmd}\n"));
        out.push_str(&decode_output(&stdout));
        let stderr = decode_output(&stderr);
        if !stderr.trim().is_empty() {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("[stderr]\n");
            out.push_str(&stderr);
        }
        let code = status.code().unwrap_or(-1);
        out.push_str(&format!("\n[exit code: {code}]"));
        truncate(&mut out, ctx.max_output_chars);
        // Post-exit notes, appended AFTER truncate so they can't be cut away.
        // The command itself ran to completion (real exit code above); these
        // are side notes about what its leftovers cost, so they never flip
        // is_error — the exit code stays the single truth for that.
        if c.reclaimed {
            out.push_str(
                "\n[reclaimed background processes that were still holding the output pipe; \
                 to leave one running, redirect its output: cmd >log 2>&1 &]",
            );
        }
        if c.leaked {
            out.push_str(
                "\n[stopped waiting for command output: leftover processes outside the \
                 process group still hold the pipe; some output may be missing]",
            );
        }
        // A cancel that landed while waiting for the leftover processes: the
        // command's own result stands (exit code above is real), the marker
        // records that the user pulled the plug on the stragglers.
        if c.cancelled {
            out.push_str("\n[command cancelled by user]");
        }

        if code == 0 {
            Ok(ToolOutput::text(out))
        } else {
            Ok(ToolOutput::error(out))
        }
    }
}

/// Read until EOF, keeping at most `cap` bytes; the rest is drained and
/// discarded so the writer never blocks and memory stays bounded.
fn read_capped<R: Read>(r: &mut R, cap: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let room = cap - buf.len();
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
    }
    buf
}

/// A reader thread's final message: which pipe it drained, and the bytes.
/// One message per reader, sent at EOF — so waiting with a deadline is
/// possible, but partial bytes are only available once EOF arrives.
enum ReaderDone {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

/// How long EOF may lag the direct child's exit before leftover background
/// processes are reclaimed (see [`collect_output`]). A command that leaves
/// nothing behind closes its write ends at exit, so normal commands never
/// wait out this grace.
const PIPE_GRACE: Duration = Duration::from_secs(2);

/// Slice for the collection loop's `recv_timeout`: bounds how long a cancel
/// can go unnoticed mid-grace (same cadence as the poll loop's 50ms).
const COLLECT_POLL: Duration = Duration::from_millis(50);

/// Output collected from the two reader threads after the poll loop, plus
/// what had to happen to get it.
struct Collected {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Had to kill the process group to force pipe EOF — leftover background
    /// processes held a write end past the grace.
    reclaimed: bool,
    /// Gave up waiting: readers abandoned mid-read, their bytes lost (a
    /// process outside the group still holds the pipe).
    leaked: bool,
    /// ctx.cancel fired while collecting.
    cancelled: bool,
}

/// Collect the reader threads' output once the poll loop is done, forcing EOF
/// when the command left background processes holding the pipes.
///
/// EOF on a pipe needs EVERY write end closed, and a backgrounded process
/// inherits the tool's stdout/stderr as its own fd1/2: `sleep 60 & echo hi`,
/// or `cd dir && server >/dev/null 2>&1 & curl …` where `&` backgrounds the
/// whole AND-list and the subshell forked for it — not the inner server,
/// whose redirect only covers itself — holds the pipe for as long as it
/// waits. The direct child's exit closes only ITS write ends, so after a
/// normal exit the readers can block in `read()` forever, and an
/// unconditional join (the pre-fix code) hung the whole turn with no rescue:
/// the timeout/cancel checks live in the poll loop, which has already exited,
/// and the run-level deadline only caps chat requests, not tool execution.
///
/// Collection is therefore phased:
/// 1. wait up to [`PIPE_GRACE`] for natural EOF (a cancel aborts at the next
///    slice, skipping straight to the reclaim);
/// 2. pipe still open → [`kill_process_group`] to force EOF — the group
///    SURVIVES the direct child's exit (reparenting to init doesn't change
///    pgid), so the kill still reaches the stragglers. Same kill the timeout
///    path uses, just triggered by pipe closure instead of time;
/// 3. still open after a second grace — a process that escaped the group
///    (`setsid`), or Windows where the non-unix "group kill" can only kill
///    the already-exited direct child — abandon the readers: return without
///    joining (two threads stay blocked in `read()` until process exit)
///    rather than hang the turn. Their bytes are lost.
///
/// Theoretical, accepted: the ONE remaining killpg-after-reap is the
/// normal-exit reclaim (phase 2 on a self-exited child) — by then its pid
/// could in principle have been reused as a new process-group id, but cycling
/// the whole pid space (macOS ~99k, Linux ~4M) inside a 2s grace is not a
/// real scenario. The timeout/cancel path passes `already_killed`, so its
/// phase 2 never killpg's the already-reaped child at all (the earlier claim
/// that this path "has no such window" was wrong — the poll loop reaps via
/// `child.wait()` right after killing). (Windows Job Objects would close the
/// phase-3 gap there; backlog.)
fn collect_output(
    rx: &std::sync::mpsc::Receiver<ReaderDone>,
    child: &mut std::process::Child,
    ctx: &ToolCtx,
    // True when the poll loop already killed the process group and reaped
    // the child (timeout / cancel). Phase 2 then skips its own killpg —
    // killing a REAPED pid could hit an unrelated process group that the OS
    // handed the reused pid — and only waits out another grace for the EOF
    // the first kill already forced.
    already_killed: bool,
) -> Collected {
    // Phase 1: bounded wait for natural EOF. On the killed path (timeout /
    // cancel) the poll loop's killpg already forced EOF, so this returns
    // immediately; on a normal exit it covers output still in flight plus
    // short-lived background writers finishing up.
    let mut cancelled = false;
    let (mut out, mut err) = wait_readers(rx, Instant::now() + PIPE_GRACE, || {
        if ctx
            .cancel
            .as_ref()
            .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
        {
            cancelled = true;
            true
        } else {
            false
        }
    });

    // Phase 2: the pipe is still open — someone in the child's group is
    // holding a write end. On a normal exit, kill the group to force EOF;
    // when the poll loop already killed (child reaped), skip the second
    // killpg (pid-reuse window) and only wait out another grace.
    let mut reclaimed = false;
    if out.is_none() || err.is_none() {
        if !already_killed {
            kill_process_group(child);
            reclaimed = true;
        }
        let (o, e) = wait_readers(rx, Instant::now() + PIPE_GRACE, || false);
        out = out.or(o);
        err = err.or(e);
    }

    // Phase 3: even the kill didn't produce EOF. Abandon the readers instead
    // of hanging; whatever they read is stuck in the held pipe.
    let leaked = out.is_none() || err.is_none();
    Collected {
        stdout: out.unwrap_or_default(),
        stderr: err.unwrap_or_default(),
        reclaimed,
        leaked,
        cancelled,
    }
}

/// Wait for the readers' EOF messages until `deadline`, waking on arrival (no
/// added latency for normal commands) and slicing the blocking wait at
/// [`COLLECT_POLL`] so `on_slice` — the cancel check — runs at least that
/// often. `on_slice` returning true aborts the wait early.
fn wait_readers(
    rx: &std::sync::mpsc::Receiver<ReaderDone>,
    deadline: Instant,
    mut on_slice: impl FnMut() -> bool,
) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let (mut out, mut err) = (None, None);
    while out.is_none() || err.is_none() {
        let remain = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remain.min(COLLECT_POLL)) {
            Ok(ReaderDone::Stdout(b)) => out = Some(b),
            Ok(ReaderDone::Stderr(b)) => err = Some(b),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline || on_slice() {
                    break;
                }
            }
            // Both readers exited without reporting (a panicked reader): the
            // bytes are gone; stop waiting instead of holding the deadline.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    (out, err)
}

/// Decode captured command output to a string. Modern tools emit UTF-8, but on
/// Windows cmd.exe (and many legacy programs) emit text in the system OEM code
/// page — GBK/936 on Chinese Windows, Shift_JIS/932 on Japanese, etc. Decoding
/// those bytes as UTF-8 turns every multi-byte sequence into U+FFFD (the
/// "'python3' 不是内部或外部命令" error becomes mojibake). Try strict UTF-8
/// first; on Windows fall back to the OEM code page so localized messages come
/// through intact. Unix tools are UTF-8, so the fallback is Windows-only.
fn decode_output(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    #[cfg(windows)]
    if let Some(s) = decode_oem(bytes) {
        return s;
    }
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(windows)]
fn decode_oem(bytes: &[u8]) -> Option<String> {
    let cp = unsafe { windows_sys::Win32::Globalization::GetOEMCP() };
    oem_encoding_for(cp).map(|enc| enc.decode(bytes).0.into_owned())
}

/// Map a Windows code page number to an encoding_rs encoding. Only the OEM
/// pages that carry multi-byte text need mapping: their output fails UTF-8
/// decode above, while ASCII OEM pages (437/850) are valid UTF-8 and never
/// reach here. Separated from `decode_oem` so the mapping is testable without
/// the FFI call (and without depending on the host machine's locale).
#[cfg(windows)]
fn oem_encoding_for(cp: u32) -> Option<&'static encoding_rs::Encoding> {
    Some(match cp {
        936 => encoding_rs::GBK,
        932 => encoding_rs::SHIFT_JIS,
        949 => encoding_rs::EUC_KR,
        950 => encoding_rs::BIG5,
        874 => encoding_rs::WINDOWS_874,
        _ => return None,
    })
}

/// Kill the child's entire process group. `process_group(0)` at spawn made
/// the child a group leader, so its pid IS the group id and `-pid` targets
/// every descendant — plain `child.kill()` only signals the direct child and
/// orphans grandchildren (a build tool's compilers, a test runner's workers).
/// The group SURVIVES the direct child's exit (reparenting to init doesn't
/// change pgid), so this also works post-exit to reclaim background processes
/// that kept the output pipes open (collect_output) — by then the child is
/// already reaped and its pid theoretically reusable as a fresh pgid, but
/// cycling the whole pid space inside the 2s grace is not a real scenario.
/// ESRCH (nothing left in the group) is harmless, so the return is ignored.
/// No process groups off unix — kill the direct child so a timeout/cancel
/// actually stops the command (a bare no-op left child.wait() blocking until
/// the command finished on its own, voiding the configured timeout and
/// Ctrl-C). On the post-exit reclaim path that kill is a no-op (child already
/// dead) and collection falls through to abandoning the readers. Mirrors
/// StdioTransport::drop's non-unix branch.
#[cfg(unix)]
fn kill_process_group(child: &mut std::process::Child) {
    let pgid = child.id() as i32;
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(child: &mut std::process::Child) {
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn oem_decode_turns_gbk_cmd_error_into_chinese() {
        // cmd.exe on Chinese Windows (OEM CP 936/GBK) emits its "'x' 不是内部
        // 或外部命令" error in GBK. UTF-8 decode of those bytes fails; the OEM
        // fallback must produce readable Chinese, not U+FFFD soup. Tests the
        // mapping + decode directly so it doesn't depend on the host's locale.
        let enc = oem_encoding_for(936).expect("CP936 maps to GBK");
        assert_eq!(enc.name(), "GBK");
        // Real bytes for "不是内部" in GBK — not valid UTF-8, so the fallback fires.
        let gbk = [0xb2u8, 0xbb, 0xca, 0xc7, 0xc4, 0xda, 0xb2, 0xbf];
        let s = enc.decode(&gbk).0;
        assert!(
            s.contains("不是内部"),
            "GBK should decode to Chinese, got: {s:?}"
        );
    }
    #[cfg(unix)]
    use std::sync::atomic::AtomicBool;
    #[cfg(unix)]
    use std::sync::Arc;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    #[test]
    fn cancel_flag_kills_a_running_command_promptly() {
        // Cancel pre-set: the poll loop must notice it on the first iteration
        // and kill the whole process group, returning within ~1s instead of
        // waiting out the full shell timeout (120s by default). The output
        // reports the cancellation distinctly from a timeout.
        let cancel = Arc::new(AtomicBool::new(true));
        let ctx = ToolCtx {
            workdir: std::env::temp_dir(),
            max_output_chars: 1000,
            restrict_to_workdir: false,
            cancel: Some(cancel),
            shell_timeout_secs: 120,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
        };
        let start = Instant::now();
        let out = Shell.run(&json!({"command": "sleep 30"}), &ctx).unwrap();
        let elapsed = start.elapsed();
        assert!(out.is_error);
        assert!(out.content.contains("cancelled by user"));
        assert!(
            elapsed < Duration::from_secs(3),
            "cancel should kill within a poll interval, took {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stdin_is_null_so_a_reading_command_gets_eof_not_a_hang() {
        // `cat` reads stdin to EOF. With Stdio::null() it sees EOF instantly
        // and exits 0; with an inherited stdin it would block the tool call
        // until the shell timeout kills it (and, under a harness, could eat
        // the harness's own piped input).
        let ctx = ToolCtx {
            workdir: std::env::temp_dir(),
            max_output_chars: 1000,
            restrict_to_workdir: false,
            cancel: None,
            shell_timeout_secs: 5,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
        };
        let out = Shell
            .run(&json!({"command": "echo seeded; cat; echo done"}), &ctx)
            .unwrap();
        assert!(!out.is_error, "cat should EOF immediately: {}", out.content);
        assert!(out.content.contains("seeded"));
        assert!(out.content.contains("done"));
    }

    /// The byte cap on stdout is sized at 4 bytes/char so the char-based
    /// truncate() still fires for CJK/emoji (3-4 bytes/char). Without that
    /// sizing, large multi-byte output falls short of the char limit at the
    /// byte cap and truncate() never appends its "[truncated]" marker — the
    /// model gets silently cut output. Emit 5000 中 chars (15000 bytes) under a
    /// 1000-char limit and require the marker: with the old byte-only cap this
    /// fails (only ~674 chars survive the byte cap, under the 1000 threshold);
    /// with the 4× cap it passes (~1674 chars survive, over the threshold).
    #[cfg(unix)]
    #[test]
    fn multibyte_output_marks_truncation_by_chars_not_bytes() {
        let ctx = ToolCtx {
            workdir: std::env::temp_dir(),
            max_output_chars: 1000,
            restrict_to_workdir: false,
            cancel: None,
            shell_timeout_secs: 120,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
        };
        let out = Shell
            .run(&json!({"command": "printf '中%.0s' $(seq 1 5000)"}), &ctx)
            .unwrap();
        assert!(
            out.content.contains("[truncated"),
            "multi-byte output must be marked truncated, got: {}",
            out.content
        );
    }

    fn ctx(workdir: std::path::PathBuf, max_output_chars: usize) -> ToolCtx {
        ToolCtx {
            workdir,
            max_output_chars,
            restrict_to_workdir: false,
            cancel: None,
            shell_timeout_secs: 120,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
        }
    }

    #[test]
    fn runs_command_and_reports_zero_exit() {
        let out = Shell
            .run(
                &json!({"command": "echo hello"}),
                &ctx(std::env::temp_dir(), 1000),
            )
            .unwrap();
        assert!(!out.is_error, "echo should exit 0: {}", out.content);
        assert!(out.content.contains("hello"));
        assert!(out.content.contains("[exit code: 0]"));
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_exit_is_flagged_error_with_code() {
        let out = Shell
            .run(
                &json!({"command": "exit 7"}),
                &ctx(std::env::temp_dir(), 1000),
            )
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("[exit code: 7]"));
    }

    #[cfg(unix)]
    #[test]
    fn stderr_is_captured_under_a_section_header() {
        let out = Shell
            .run(
                &json!({"command": "echo oops >&2"}),
                &ctx(std::env::temp_dir(), 1000),
            )
            .unwrap();
        assert!(
            out.content.contains("[stderr]"),
            "stderr section missing: {}",
            out.content
        );
        assert!(out.content.contains("oops"));
    }

    #[cfg(unix)]
    #[test]
    fn runs_in_the_configured_working_directory() {
        let dir = std::env::temp_dir().join(format!("ta-shell-wd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = Shell
            .run(&json!({"command": "pwd"}), &ctx(dir.clone(), 1000))
            .unwrap();
        // pwd may resolve a symlink (e.g. /tmp → /private/tmp on macOS);
        // accept either spelling.
        let canonical = dir.canonicalize().unwrap();
        assert!(
            out.content.contains(dir.to_string_lossy().as_ref())
                || out.content.contains(canonical.to_string_lossy().as_ref()),
            "pwd should run in the configured workdir, got: {}",
            out.content
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn ascii_output_marks_truncation() {
        // ~1000 chars under a 50-char cap → the char-based truncate() must fire.
        let out = Shell
            .run(
                &json!({"command": "printf 'x%.0s' $(seq 1 1000)"}),
                &ctx(std::env::temp_dir(), 50),
            )
            .unwrap();
        assert!(out.content.contains("[truncated"), "got: {}", out.content);
    }

    #[cfg(unix)]
    #[test]
    fn configured_timeout_kills_a_long_command_promptly() {
        // shell_timeout_secs is a real knob: 1s must kill `sleep 30` within a
        // poll interval, report an error, and name the configured timeout.
        let ctx = ToolCtx {
            workdir: std::env::temp_dir(),
            max_output_chars: 1000,
            restrict_to_workdir: false,
            cancel: None,
            shell_timeout_secs: 1,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
        };
        let start = Instant::now();
        let out = Shell.run(&json!({"command": "sleep 30"}), &ctx).unwrap();
        let elapsed = start.elapsed();
        assert!(out.is_error);
        assert!(
            out.content
                .contains("[command timed out after 1s and was killed]"),
            "got: {}",
            out.content
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "1s timeout should kill within a poll interval, took {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn zero_timeout_runs_long_commands_to_completion() {
        // 0 = no per-command limit: a 2s command finishes normally instead of
        // being cut by the default 120s (which wouldn't fire here) — and more
        // importantly the Option<Instant> deadline must not treat 0 as "now".
        let ctx = ToolCtx {
            workdir: std::env::temp_dir(),
            max_output_chars: 1000,
            restrict_to_workdir: false,
            cancel: None,
            shell_timeout_secs: 0,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
        };
        let out = Shell
            .run(&json!({"command": "sleep 2 && echo done"}), &ctx)
            .unwrap();
        assert!(!out.is_error, "got: {}", out.content);
        assert!(out.content.contains("done"));
    }

    // ── Post-exit pipe reclamation (the orphan-hang fix) ─────────────────────
    //
    // Assertions match "\npayload" (payload at line start) rather than the
    // bare word: the output's first line echoes the command itself, which
    // contains every payload string.

    #[cfg(unix)]
    #[test]
    fn background_process_holding_the_pipe_is_reclaimed() {
        // `sleep 60 &` inherits the tool's pipes as fd1/2 and never closes
        // them: the direct sh exits immediately, EOF never arrives, and the
        // pre-fix unconditional join hung the turn FOREVER (no timeout /
        // cancel check survived on that path). The grace must expire, the
        // group must be reclaimed (the note says so), and the call must come
        // back in ~grace — not 60s.
        let start = Instant::now();
        let out = Shell
            .run(
                &json!({"command": "sleep 60 & echo bg_here"}),
                &ctx(std::env::temp_dir(), 5000),
            )
            .unwrap();
        let elapsed = start.elapsed();
        assert!(
            !out.is_error,
            "echo exits 0; the reclaim note must not flip error: {}",
            out.content
        );
        assert!(out.content.contains("\nbg_here"), "got: {}", out.content);
        assert!(out.content.contains("[exit code: 0]"));
        assert!(
            out.content.contains("[reclaimed"),
            "reclaim note missing: {}",
            out.content
        );
        assert!(
            elapsed < Duration::from_secs(6),
            "grace (2s) + slack, took {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn backgrounded_and_list_subshell_is_reclaimed() {
        // The reporter's shape: `&` backgrounds the whole AND-list, and the
        // subshell forked for it holds the pipes as fd1/2 — the inner command
        // could redirect all it wants, the subshell itself still holds the
        // write ends while it waits. Same reclaim as the simple case.
        let start = Instant::now();
        let out = Shell
            .run(
                &json!({"command": "sleep 60 && echo never & echo done_now"}),
                &ctx(std::env::temp_dir(), 5000),
            )
            .unwrap();
        assert!(out.content.contains("\ndone_now"), "got: {}", out.content);
        assert!(
            out.content.contains("[reclaimed"),
            "reclaim note missing: {}",
            out.content
        );
        assert!(
            start.elapsed() < Duration::from_secs(6),
            "took {:?}",
            start.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn redirected_background_process_does_not_hold_the_pipe() {
        // The documented escape hatch: a background process that redirects
        // its own output never holds the tool's pipes, so the tool must NOT
        // wait out the grace or fire the reclaim — this is how a model leaves
        // something running across calls.
        let start = Instant::now();
        let out = Shell
            .run(
                &json!({"command": "sleep 60 >/dev/null 2>&1 & echo gone_now"}),
                &ctx(std::env::temp_dir(), 5000),
            )
            .unwrap();
        assert!(out.content.contains("\ngone_now"), "got: {}", out.content);
        assert!(
            !out.content.contains("[reclaimed"),
            "a redirected bg process must not be reclaimed: {}",
            out.content
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "EOF should arrive at child exit, not after the grace: {:?}",
            start.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancel_during_grace_reclaims_immediately() {
        // Ctrl-C while collection is inside the grace: the wait must abort at
        // the next slice, kill the group, and return with the cancel marker —
        // long before the 2s grace would have expired on its own.
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let ctx = ToolCtx {
            cancel: Some(cancel),
            ..ctx(std::env::temp_dir(), 5000)
        };
        let start = Instant::now();
        let out = Shell
            .run(&json!({"command": "sleep 60 & echo ccl_now"}), &ctx)
            .unwrap();
        assert!(out.content.contains("\nccl_now"), "got: {}", out.content);
        assert!(
            out.content.contains("[command cancelled by user]"),
            "cancel marker missing: {}",
            out.content
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "cancel at ~300ms must beat the 2s grace: {:?}",
            start.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn group_escapee_abandons_readers_instead_of_hanging() {
        // os.setsid moves the background process into its own session, so the
        // reclaim kill can't force EOF. The collector must give up (abandon
        // the blocked readers, report the loss) instead of hanging: both
        // graces expire, so this returns after ~4s with the stopped-waiting
        // note and WITHOUT the pipe's content (it is stuck behind the
        // escapee). Depends on python3 (CI's unix images carry it for the
        // bindings; there is no `setsid(1)` binary on macOS to do this in sh).
        let start = Instant::now();
        let out = Shell
            .run(
                &json!({"command": "python3 -c 'import os,time; os.setsid(); time.sleep(60)' & echo lost_now"}),
                &ctx(std::env::temp_dir(), 5000),
            )
            .unwrap();
        assert!(
            out.content.contains("[stopped waiting"),
            "abandon note missing: {}",
            out.content
        );
        assert!(
            !out.content.contains("\nlost_now"),
            "stdout is stuck in the held pipe and must be reported lost: {}",
            out.content
        );
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "two graces (2s+2s) + spawn, took {:?}",
            start.elapsed()
        );
    }
}
