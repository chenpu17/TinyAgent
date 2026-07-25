mod file;
mod image;
pub(crate) mod mcp;
mod shell;

pub(crate) use image::load_block;

use crate::provider::{Block, ToolDef};
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

pub struct ToolOutput {
    pub content: String,
    pub image: Option<Block>,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: s.into(),
            image: None,
            is_error: false,
        }
    }

    pub fn error(s: impl Into<String>) -> Self {
        Self {
            content: s.into(),
            image: None,
            is_error: true,
        }
    }
}

pub trait Tool: Send + Sync {
    fn def(&self) -> ToolDef;
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput>;
}

/// Instantiate the built-in tools named in the config whitelist.
pub fn build(names: &[String]) -> Result<Vec<Box<dyn Tool>>> {
    let mut out: Vec<Box<dyn Tool>> = Vec::new();
    for name in names {
        out.push(match name.as_str() {
            "shell" => Box::new(shell::Shell),
            "read_file" => Box::new(file::ReadFile),
            "write_file" => Box::new(file::WriteFile),
            "list_dir" => Box::new(file::ListDir),
            "load_image" => Box::new(image::LoadImage),
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
