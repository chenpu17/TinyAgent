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
        let cap = ctx.max_output_chars.saturating_add(1024);
        let mut out_pipe = child.stdout.take().expect("stdout piped");
        let mut err_pipe = child.stderr.take().expect("stderr piped");
        let out_thread = std::thread::spawn(move || read_capped(&mut out_pipe, cap));
        let err_thread = std::thread::spawn(move || read_capped(&mut err_pipe, cap));

        // Poll the child so a timeout can kill it (wait_with_output would
        // consume the handle and leak the process on timeout).
        let deadline = Instant::now() + CMD_TIMEOUT;
        let status = loop {
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
            truncate(&mut out, ctx.max_output_chars);
            out.push_str(&format!(
                "\n[command timed out after {}s and was killed]",
                CMD_TIMEOUT.as_secs()
            ));
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
/// ESRCH (already reaped) is harmless, so the return is ignored. No-op off unix.
#[cfg(unix)]
fn kill_process_group(child: &std::process::Child) {
    let pgid = child.id() as i32;
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_child: &std::process::Child) {}
