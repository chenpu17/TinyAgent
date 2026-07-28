mod file;
mod image;
pub(crate) mod mcp;
mod shell;
mod subagent;

pub(crate) use image::load_block;

use crate::agent::AgentEvent;
use crate::config::Config;
use crate::provider::{Block, ToolDef, Usage};
use anyhow::{bail, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Shared context passed to every tool invocation.
pub struct ToolCtx {
    pub workdir: PathBuf,
    pub max_output_chars: usize,
    /// When true, file tools refuse paths that escape `workdir`.
    pub restrict_to_workdir: bool,
    /// Cancellation flag shared with the agent run. Pollable tools (shell)
    /// check this between waits so Ctrl-C can interrupt a long-running
    /// command within ~50ms instead of waiting out its full timeout. None in
    /// headless mode (no interactive cancellation).
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

#[derive(Debug)]
pub struct ToolOutput {
    pub content: String,
    pub image: Option<Block>,
    pub is_error: bool,
    /// Token usage a tool wants billed to the parent run. Leaf tools (shell,
    /// file, …) leave this None; the `task` sub-agent tool sets it to its
    /// child run's usage so sub-agent cost is counted honestly in the parent's
    /// totals (and surfaces via AgentEvent::Usage / result.json).
    pub usage: Option<Usage>,
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: s.into(),
            image: None,
            is_error: false,
            usage: None,
        }
    }

    pub fn error(s: impl Into<String>) -> Self {
        Self {
            content: s.into(),
            image: None,
            is_error: true,
            usage: None,
        }
    }
}

pub trait Tool: Send + Sync {
    fn def(&self) -> ToolDef;
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput>;
    /// Run with an event sink, for tools that stream progress into the parent
    /// run (the `task` sub-agent forwards its child's `AgentEvent`s, wrapped
    /// with the sub-agent's identity). Leaf tools have nothing to emit and use
    /// the default, which ignores `emit` and delegates to `run` — so adding
    /// this didn't touch the 6 leaf impls. The agent loop always calls this;
    /// the parallel path (commit 2) gives each worker its own `emit` closure
    /// backed by an mpsc sender.
    fn run_with_emit(
        &self,
        input: &Value,
        ctx: &ToolCtx,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<ToolOutput> {
        let _ = emit;
        self.run(input, ctx)
    }
}

/// Instantiate the built-in tools named in the config whitelist.
///
/// `cfg` / `client` / `api_key` are only used by the `task` sub-agent tool
/// (which builds a fresh child provider+agent); the leaf tools ignore them.
/// Passing them here keeps the `Tool` trait's `run(ctx)` signature free of
/// agent-construction plumbing.
pub fn build(
    names: &[String],
    cfg: &Config,
    client: &reqwest::blocking::Client,
    api_key: &str,
) -> Result<Vec<Box<dyn Tool>>> {
    let mut out: Vec<Box<dyn Tool>> = Vec::new();
    for name in names {
        out.push(match name.as_str() {
            "shell" => Box::new(shell::Shell),
            "read_file" => Box::new(file::ReadFile),
            "write_file" => Box::new(file::WriteFile),
            "edit_file" => Box::new(file::EditFile),
            "list_dir" => Box::new(file::ListDir),
            "load_image" => Box::new(image::LoadImage),
            "task" => Box::new(subagent::SubAgent::new(
                cfg.clone(),
                client.clone(),
                api_key.to_string(),
            )),
            other => bail!("unknown tool in config: {other}"),
        });
    }
    Ok(out)
}

/// Truncate to `max` chars on a char boundary, noting the omission.
pub(crate) fn truncate(s: &mut String, max: usize) {
    if s.chars().count() > max {
        let byte_idx = s
            .char_indices()
            .nth(max)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        s.truncate(byte_idx);
        s.push_str(&format!("\n... [truncated, limit {max} chars]"));
    }
}

/// Resolve a user-supplied path against the working directory. When
/// `restrict_to_workdir` is on, paths escaping the workdir (via `../`
/// or absolute paths) are rejected.
pub(crate) fn resolve(ctx: &ToolCtx, path: &str) -> Result<PathBuf> {
    let p = PathBuf::from(path);
    let full = if p.is_absolute() {
        p
    } else {
        ctx.workdir.join(p)
    };
    if ctx.restrict_to_workdir {
        return confine(ctx, &full);
    }
    Ok(full)
}

/// Check that `full` stays inside the workdir. Canonicalizes the deepest
/// existing ancestor (so writes to not-yet-existing files are checked too)
/// and re-appends the remainder; the workdir itself is canonicalized at
/// config load, so a plain prefix comparison is sound.
fn confine(ctx: &ToolCtx, full: &Path) -> Result<PathBuf> {
    let mut ancestor: &Path = full;
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(mut resolved) => {
                for comp in rest.iter().rev() {
                    resolved.push(comp);
                }
                if resolved.starts_with(&ctx.workdir) {
                    return Ok(resolved);
                }
                bail!(
                    "path escapes the working directory (restrict_to_workdir): {}",
                    full.display()
                );
            }
            Err(_) => match ancestor.file_name() {
                Some(name) => {
                    rest.push(name.to_os_string());
                    ancestor = ancestor
                        .parent()
                        .ok_or_else(|| anyhow::anyhow!("cannot resolve path: {}", full.display()))?;
                }
                None => bail!("cannot resolve path: {}", full.display()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_marks_omission() {
        let mut s = "x".repeat(5000);
        truncate(&mut s, 100);
        assert!(s.chars().count() < 200);
        assert!(s.contains("[truncated, limit 100 chars]"));
    }
}
