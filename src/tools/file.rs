use super::{resolve, truncate, Tool, ToolCtx, ToolOutput};
use crate::provider::ToolDef;
use anyhow::{bail, Context, Result};
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
        let full = resolve(ctx, path)?;
        // Stream line by line and stop once the output cap is hit, so a
        // huge file never gets pulled into memory whole.
        let f = std::fs::File::open(&full)
            .with_context(|| format!("cannot read {}", full.display()))?;
        let reader = std::io::BufReader::new(f);

        let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = input["limit"].as_u64().map(|n| n as usize);

        use std::io::BufRead;
        let mut out = String::new();
        let mut lines_seen = 0usize;
        for (i, line) in reader.lines().enumerate() {
            lines_seen += 1;
            if i < offset - 1 {
                continue;
            }
            if let Some(l) = limit {
                if i - (offset - 1) >= l {
                    out.push_str("... [line limit reached]\n");
                    break;
                }
            }
            let line = line.with_context(|| format!("cannot read {}", full.display()))?;
            out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
            if out.len() >= ctx.max_output_chars {
                truncate(&mut out, ctx.max_output_chars);
                break;
            }
        }
        if out.is_empty() {
            // Distinguish a truly empty file from an offset past EOF. The
            // latter used to read as "[empty file]", which misleads the
            // model into thinking the file has no content.
            if lines_seen == 0 {
                out.push_str("[empty file]");
            } else {
                out.push_str(&format!(
                    "... [offset {offset} beyond EOF; file has {lines_seen} lines]\n"
                ));
            }
        }
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
        let full = resolve(ctx, path)?;
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        std::fs::write(&full, content)
            .with_context(|| format!("cannot write {}", full.display()))?;
        // Echo the path as given (usually relative) — less confusing than
        // the canonicalized absolute form.
        Ok(ToolOutput::text(format!(
            "wrote {path} ({} bytes)",
            content.len()
        )))
    }
}

pub struct EditFile;

impl Tool for EditFile {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "edit_file".into(),
            description: "Edit a file by replacing an exact occurrence of `old_string` with \
                          `new_string`. Read the file first to copy exact text (including \
                          indentation). By default `old_string` must appear exactly once; set \
                          `replace_all: true` to replace every occurrence. To create a file or \
                          rewrite it wholesale, use write_file."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string", "description": "Exact text to find"},
                    "new_string": {"type": "string", "description": "Text to replace it with"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false; otherwise old_string must be unique)"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path = input["path"].as_str().context("edit_file: missing \"path\"")?;
        let old_string = input["old_string"]
            .as_str()
            .context("edit_file: missing \"old_string\"")?;
        let new_string = input["new_string"]
            .as_str()
            .context("edit_file: missing \"new_string\"")?;
        let replace_all = input["replace_all"].as_bool().unwrap_or(false);

        if old_string.is_empty() {
            bail!("edit_file: old_string is empty; use write_file to create a file");
        }
        if old_string == new_string {
            bail!("edit_file: old_string and new_string are identical (no change)");
        }

        // resolve() honors restrict_to_workdir for both the read and the write
        // (same sandbox as read_file/write_file); a path escaping the workdir is
        // rejected before any I/O.
        let full = resolve(ctx, path)?;
        let original = std::fs::read_to_string(&full)
            .with_context(|| format!("cannot read {}", full.display()))?;

        let count = original.matches(old_string).count();
        if count == 0 {
            bail!(
                "edit_file: old_string not found in {path} ({} bytes) — re-read the file for exact text",
                original.len()
            );
        }
        if count > 1 && !replace_all {
            bail!(
                "edit_file: old_string matches {count} times in {path}; include more surrounding context, or set replace_all: true"
            );
        }

        let updated = if replace_all {
            original.replace(old_string, new_string)
        } else {
            original.replacen(old_string, new_string, 1)
        };
        // Atomic (temp + fsync + rename): unlike write_file — which the model
        // can redo from its in-context copy — edit_file mutates an existing file
        // the model may NOT have in full, so a crash mid-write (panic=abort,
        // SIGKILL, power loss) must not leave it truncated. Same helper
        // result.json/report.json use.
        crate::harness::atomic_write(&full, updated.as_bytes())
            .with_context(|| format!("cannot write {}", full.display()))?;
        let n = if replace_all { count } else { 1 };
        Ok(ToolOutput::text(format!(
            "edited {path} ({n} replacement{}; {} → {} bytes)",
            if n == 1 { "" } else { "s" },
            original.len(),
            updated.len()
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
        let full = resolve(ctx, path)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn ctx_in(dir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            workdir: dir.to_path_buf(),
            max_output_chars: 1000,
            restrict_to_workdir: false,
            cancel: None,
        }
    }

    #[test]
    fn read_file_offset_beyond_eof_reports_range() {
        let dir = std::env::temp_dir().join(format!("ta-read-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "line1\nline2\n").unwrap();
        let out = ReadFile
            .run(&json!({"path": "f.txt", "offset": 99}), &ctx_in(&dir))
            .unwrap();
        assert!(out.content.contains("beyond EOF"), "got: {}", out.content);
        assert!(out.content.contains("2 lines"), "got: {}", out.content);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_file_truly_empty() {
        let dir = std::env::temp_dir().join(format!("ta-empty-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("empty.txt"), "").unwrap();
        let out = ReadFile
            .run(&json!({"path": "empty.txt"}), &ctx_in(&dir))
            .unwrap();
        assert_eq!(out.content, "[empty file]");
        fs::remove_dir_all(&dir).ok();
    }

    fn ctx_restricted(dir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            workdir: dir.to_path_buf(),
            max_output_chars: 1000,
            restrict_to_workdir: true,
            cancel: None,
        }
    }

    #[test]
    fn edit_file_unique_replace_succeeds() {
        let dir = std::env::temp_dir().join(format!("ta-edit-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let out = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "beta", "new_string": "BETA"}),
                &ctx_in(&dir),
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("1 replacement"), "{}", out.content);
        assert_eq!(fs::read_to_string(dir.join("f.txt")).unwrap(), "alpha\nBETA\ngamma\n");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edit_file_not_found_errors() {
        let dir = std::env::temp_dir().join(format!("ta-edit-nf-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "alpha\n").unwrap();
        let err = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "zzz", "new_string": "y"}),
                &ctx_in(&dir),
            )
            .unwrap_err();
        assert!(format!("{err:#}").contains("not found"), "{err:#}");
        // File unchanged on failure.
        assert_eq!(fs::read_to_string(dir.join("f.txt")).unwrap(), "alpha\n");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edit_file_multiple_matches_errors_without_replace_all() {
        let dir = std::env::temp_dir().join(format!("ta-edit-mm-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "x x x\n").unwrap();
        let err = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "x", "new_string": "y"}),
                &ctx_in(&dir),
            )
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("3 times"), "{msg}");
        assert_eq!(fs::read_to_string(dir.join("f.txt")).unwrap(), "x x x\n");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edit_file_replace_all_replaces_every() {
        let dir = std::env::temp_dir().join(format!("ta-edit-all-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "x x x\n").unwrap();
        let out = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "x", "new_string": "y", "replace_all": true}),
                &ctx_in(&dir),
            )
            .unwrap();
        assert!(out.content.contains("3 replacements"), "{}", out.content);
        assert_eq!(fs::read_to_string(dir.join("f.txt")).unwrap(), "y y y\n");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edit_file_rejects_empty_and_identical_old_string() {
        let dir = std::env::temp_dir().join(format!("ta-edit-ei-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "hello\n").unwrap();
        let empty_err = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "", "new_string": "y"}),
                &ctx_in(&dir),
            )
            .unwrap_err();
        assert!(format!("{empty_err:#}").contains("empty"));
        let same_err = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "hello", "new_string": "hello"}),
                &ctx_in(&dir),
            )
            .unwrap_err();
        assert!(format!("{same_err:#}").contains("identical"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edit_file_rejects_escape_under_restrict() {
        let dir = std::env::temp_dir().join(format!("ta-edit-esc-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("ok.txt"), "hello").unwrap();
        // An absolute path outside the workdir must be rejected before any I/O
        // when restrict_to_workdir is on (same sandbox as write_file).
        let outside = std::env::temp_dir().join(format!("ta-edit-out-{}.txt", std::process::id()));
        fs::write(&outside, "hello").unwrap();
        let res = EditFile.run(
            &json!({"path": outside.to_string_lossy(), "old_string": "hello", "new_string": "bye"}),
            &ctx_restricted(&dir),
        );
        assert!(res.is_err(), "escape must be rejected under restrict_to_workdir");
        // The outside file is untouched.
        assert_eq!(fs::read_to_string(&outside).unwrap(), "hello");
        fs::remove_dir_all(&dir).ok();
        let _ = fs::remove_file(&outside);
    }
}
