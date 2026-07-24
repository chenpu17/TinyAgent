use super::{resolve, truncate, Tool, ToolCtx, ToolOutput};
use crate::provider::ToolDef;
use anyhow::{Context, Result};
use serde_json::{json, Value};

const MAX_LIST_ENTRIES: usize = 500;

pub struct ReadFile;

impl Tool for ReadFile {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "read_file".into(),
            description: "Read a text file with line numbers (path relative to the working directory).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "description": "First line to read (1-based, optional)"},
                    "limit": {"type": "integer", "description": "Max number of lines (optional)"}
                },
                "required": ["path"]
            }),
        }
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path = input["path"].as_str().context("read_file: missing \"path\"")?;
        let full = resolve(ctx, path);
        let raw = std::fs::read_to_string(&full)
            .with_context(|| format!("cannot read {}", full.display()))?;

        let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = input["limit"].as_u64().map(|n| n as usize);

        let mut out = String::new();
        for (i, line) in raw.lines().enumerate().skip(offset - 1) {
            if let Some(l) = limit {
                if i - (offset - 1) >= l {
                    out.push_str("... [line limit reached]\n");
                    break;
                }
            }
            out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
        }
        if out.is_empty() {
            out.push_str("[empty file]");
        }
        truncate(&mut out, ctx.max_output_chars);
        Ok(ToolOutput::text(out))
    }
}

pub struct WriteFile;

impl Tool for WriteFile {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "write_file".into(),
            description: "Write (overwrite) a file with the given full content. \
                          Parent directories are created automatically."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string", "description": "Complete new file content"}
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path = input["path"].as_str().context("write_file: missing \"path\"")?;
        let content = input["content"]
            .as_str()
            .context("write_file: missing \"content\"")?;
        let full = resolve(ctx, path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        std::fs::write(&full, content)
            .with_context(|| format!("cannot write {}", full.display()))?;
        Ok(ToolOutput::text(format!(
            "wrote {} ({} bytes)",
            full.display(),
            content.len()
        )))
    }
}

pub struct ListDir;

impl Tool for ListDir {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "list_dir".into(),
            description: "List entries of a directory (path relative to the working directory).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory path, default \".\""}
                }
            }),
        }
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path = input["path"].as_str().unwrap_or(".");
        let full = resolve(ctx, path);
        let mut entries: Vec<String> = Vec::new();
        for e in std::fs::read_dir(&full)
            .with_context(|| format!("cannot list {}", full.display()))?
        {
            let e = e?;
            let suffix = if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                "/"
            } else {
                ""
            };
            entries.push(format!("{}{}", e.file_name().to_string_lossy(), suffix));
            if entries.len() >= MAX_LIST_ENTRIES {
                entries.push(format!("... [more than {MAX_LIST_ENTRIES} entries]"));
                break;
            }
        }
        entries.sort();
        if entries.is_empty() {
            return Ok(ToolOutput::text("[empty directory]"));
        }
        let mut out = entries.join("\n");
        truncate(&mut out, ctx.max_output_chars);
        Ok(ToolOutput::text(out))
    }
}
