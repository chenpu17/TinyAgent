use super::{truncate, Tool, ToolCtx, ToolOutput};
use crate::provider::ToolDef;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::process::Command;
use std::time::{Duration, Instant};

/// Max wall-clock time for a single shell command.
const CMD_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Shell;

impl Tool for Shell {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "shell".into(),
            description: "Run a shell command in the working directory and return \
                          stdout+stderr. Use for builds, tests, git, and any CLI work."
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
        let mut child = {
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                Command::new("sh")
                    .args(["-c", cmd])
                    .current_dir(&ctx.workdir)
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
        let out_thread = std::thread::spawn(move || read_capped(&mut out_pipe, cap));
        let err_thread = std::thread::spawn(move || read_capped(&mut err_pipe, cap));

        // Poll the child so a timeout or cancel can kill it (wait_with_output
        // would consume the handle and leak the process on timeout).
        let deadline = Instant::now() + CMD_TIMEOUT;
        let mut cancelled = false;
        let status = loop {
            // Honor an interactive cancel (Ctrl-C in the TUI) between polls,
            // killing the whole process group the same way a timeout does.
            if let Some(c) = &ctx.cancel {
                if c.load(std::sync::atomic::Ordering::Relaxed) {
                    cancelled = true;
                    kill_process_group(&child);
                    let _ = child.wait();
                    break None;
                }
            }
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) if Instant::now() >= deadline => {
                    kill_process_group(&child);
                    let _ = child.wait();
                    break None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => {
                    kill_process_group(&child);
                    let _ = child.wait();
                    return Err(e).context("shell: failed to wait on command");
                }
            }
        };

        let stdout = out_thread.join().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default();

        let Some(status) = status else {
            let mut out = format!("$ {cmd}\n");
            out.push_str(&String::from_utf8_lossy(&stdout));
            // Keep stderr on the killed path too: the diagnostics a command
            // printed before hanging are exactly what's needed to see WHY it
            // timed out — dropping them makes timeouts undebuggable.
            let stderr = String::from_utf8_lossy(&stderr);
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
                    CMD_TIMEOUT.as_secs()
                ));
            }
            return Ok(ToolOutput::error(out));
        };

        let mut out = String::new();
        out.push_str(&format!("$ {cmd}\n"));
        out.push_str(&String::from_utf8_lossy(&stdout));
        let stderr = String::from_utf8_lossy(&stderr);
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

/// Kill the child's entire process group. `process_group(0)` at spawn made
/// the child a group leader, so its pid IS the group id and `-pid` targets
/// every descendant — plain `child.kill()` only signals the direct child and
/// orphans grandchildren (a build tool's compilers, a test runner's workers).
/// ESRCH (already reaped) is harmless, so the return is ignored. No process
/// groups off unix — kill the direct child so a timeout/cancel actually stops
/// the command (a bare no-op left child.wait() blocking until the command
/// finished on its own, voiding the 120s timeout and Ctrl-C). Mirrors
/// StdioTransport::drop's non-unix branch.
#[cfg(unix)]
fn kill_process_group(child: &std::process::Child) {
    let pgid = child.id() as i32;
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(child: &std::process::Child) {
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn cancel_flag_kills_a_running_command_promptly() {
        // Cancel pre-set: the poll loop must notice it on the first iteration
        // and kill the whole process group, returning within ~1s instead of
        // waiting out the full CMD_TIMEOUT (120s). The output reports the
        // cancellation distinctly from a timeout.
        let cancel = Arc::new(AtomicBool::new(true));
        let ctx = ToolCtx {
            workdir: std::env::temp_dir(),
            max_output_chars: 1000,
            restrict_to_workdir: false,
            cancel: Some(cancel),
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
}
