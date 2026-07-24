use super::{truncate, Tool, ToolCtx, ToolOutput};
use crate::provider::ToolDef;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::process::Command;
use std::time::Duration;

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

        let child = if cfg!(windows) {
            Command::new("cmd")
                .args(["/C", cmd])
                .current_dir(&ctx.workdir)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
        } else {
            Command::new("sh")
                .args(["-c", cmd])
                .current_dir(&ctx.workdir)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
        }
        .with_context(|| format!("failed to spawn shell for: {cmd}"))?;

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(child.wait_with_output()));

        let output = match rx.recv_timeout(CMD_TIMEOUT) {
            Ok(res) => res.context("shell: failed to read command output")?,
            Err(_) => {
                return Ok(ToolOutput::error(format!(
                    "command timed out after {}s: {cmd}",
                    CMD_TIMEOUT.as_secs()
                )))
            }
        };

        let mut out = String::new();
        out.push_str(&format!("$ {cmd}\n"));
        out.push_str(&String::from_utf8_lossy(&output.stdout));
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("[stderr]\n");
            out.push_str(&stderr);
        }
        let code = output.status.code().unwrap_or(-1);
        out.push_str(&format!("\n[exit code: {code}]"));
        truncate(&mut out, ctx.max_output_chars);

        if code == 0 {
            Ok(ToolOutput::text(out))
        } else {
            Ok(ToolOutput::error(out))
        }
    }
}
