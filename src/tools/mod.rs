mod file;
mod image;
pub(crate) mod mcp;
mod shell;

pub(crate) use image::load_block;

use crate::provider::{Block, ToolDef};
use anyhow::{bail, Result};
use serde_json::Value;
use std::path::PathBuf;

/// Shared context passed to every tool invocation.
pub struct ToolCtx {
    pub workdir: PathBuf,
    pub max_output_chars: usize,
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

/// Resolve a user-supplied path against the working directory.
pub(crate) fn resolve(ctx: &ToolCtx, path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        ctx.workdir.join(p)
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
