use super::{resolve, truncate, Tool, ToolCtx, ToolOutput};
use crate::provider::ToolDef;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::Read;

const MAX_LIST_ENTRIES: usize = 500;
/// edit_file reads the whole file into memory (replace semantics), unlike
/// read_file which streams. Refuse a file beyond this cap rather than OOM on
/// a multi-GB file (e.g. a log in the workdir). Module-level so a test can
/// reference the real value instead of a drift-prone literal.
const EDIT_FILE_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Upper bound on a replace_all RESULT: the input is capped at EDIT_FILE_MAX_BYTES,
/// but many short matches expanded into a large new_string can otherwise produce a
/// multi-GB string and OOM before it ever hits disk. The estimate is checked BEFORE
/// the allocation.
const EDIT_FILE_OUTPUT_MAX: u128 = 64 * 1024 * 1024;

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
        // Bound the BYTES read from disk so a newline-less huge file can't OOM
        // on a single line allocation — reader.lines() buffers a whole line
        // into memory before the per-line truncation below can throw it away,
        // so without this cap a multi-GB single-line file defeats the "bounded
        // read". 4× the char cap + slack mirrors the shell tool's byte budget:
        // ample for the output window, and no single line can exceed it.
        let max_bytes = (ctx.max_output_chars.saturating_mul(4)).saturating_add(1024) as u64;
        let reader = reader.take(max_bytes);

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
            // Cap a pathological single line BEFORE formatting+pushing: a
            // multi-MB line would otherwise be allocated into `out` in full
            // (far past max_output_chars) only for the truncation below to
            // throw it away. Cheap byte-length gate; char-boundary-safe cap.
            let remaining = ctx.max_output_chars.saturating_sub(out.len());
            if line.len() > remaining {
                let mut capped = line;
                truncate(&mut capped, remaining);
                out.push_str(&format!("{:>6}\t{}\n", i + 1, capped));
            } else {
                out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
            }
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
        // Atomic write (temp + fsync + rename), matching edit_file: a crash or
        // SIGKILL mid-write_file must not leave a truncated target file. The
        // previous std::fs::write overwrote in place, so a half-written file
        // survived the interruption — and once compression reclaims the
        // write_file's content from history, the model can't redo the write
        // from an in-context copy either. atomic_write also creates the parent
        // dir, so the explicit create_dir_all above is dropped.
        crate::harness::atomic_write(&full, content.as_bytes())
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
        // edit_file needs the whole file in memory (replace semantics), unlike
        // read_file which streams and stops at max_output_chars. Refuse a file
        // beyond the module-level cap (EDIT_FILE_MAX_BYTES) rather than OOM on
        // a multi-GB file (e.g. a log in the workdir). Read with a `take` cap
        // instead of metadata()-then-read: the metadata check had a
        // check-then-read race (a file growing between the two calls slipped
        // past the limit), the same pattern load_image's take() already avoids.
        let mut bytes = Vec::new();
        std::fs::File::open(&full)
            .with_context(|| format!("cannot read {}", full.display()))?
            .take(EDIT_FILE_MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("cannot read {}", full.display()))?;
        if bytes.len() as u64 > EDIT_FILE_MAX_BYTES {
            bail!(
                "edit_file: {} is over the {EDIT_FILE_MAX_BYTES}-byte limit (edit_file reads the whole file into memory) — use read_file to inspect it, or a streaming editor",
                full.display(),
            );
        }
        let original = String::from_utf8(bytes)
            .with_context(|| format!("{} is not valid UTF-8", full.display()))?;

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

        // Guard replace_all's RESULT size: original is ≤8 MiB, but many short
        // matches replaced by a large new_string can balloon the output into a
        // multi-GB allocation (and OOM) before it hits disk. Estimate without
        // allocating and bail when it would exceed the cap.
        if replace_all {
            let est = (original.len() as u128)
                .saturating_add((count as u128).saturating_mul(new_string.len() as u128))
                .saturating_sub((count as u128).saturating_mul(old_string.len() as u128));
            if est > EDIT_FILE_OUTPUT_MAX {
                bail!(
                    "edit_file: replace_all would produce ~{est} bytes ({count} × {:?} → {:?}); over the {EDIT_FILE_OUTPUT_MAX}-byte result limit — narrow old_string or edit in smaller chunks",
                    old_string,
                    new_string
                );
            }
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
        let mut overflow = false;
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
                overflow = true;
                break;
            }
        }
        entries.sort();
        // The overflow marker goes AFTER the sort — inside the loop it sorted
        // as "..." (byte-wise before most file names) and ended up at the TOP
        // of the listing, masquerading as a directory entry.
        if overflow {
            entries.push(format!("... [more than {MAX_LIST_ENTRIES} entries]"));
        }
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

            shell_timeout_secs: 120,
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

    #[test]
    fn read_file_caps_a_pathological_single_line() {
        // A single line longer than the whole output budget (minified blob,
        // base64) must be capped before formatting — not allocated in full
        // then truncated. The output stays within max_output_chars (1000
        // here) and carries the truncation marker.
        let dir = std::env::temp_dir().join(format!("ta-bigline-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let big = "x".repeat(50_000); // one line, no newline, 50× the budget
        fs::write(dir.join("big.txt"), &big).unwrap();
        let out = ReadFile
            .run(&json!({"path": "big.txt"}), &ctx_in(&dir))
            .unwrap();
        assert!(
            out.content.len() < 2000,
            "a pathological line must be capped, got {} bytes",
            out.content.len()
        );
        assert!(out.content.contains("truncated"), "got: {}", out.content);
        fs::remove_dir_all(&dir).ok();
    }

    fn ctx_restricted(dir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            workdir: dir.to_path_buf(),
            max_output_chars: 1000,
            restrict_to_workdir: true,
            cancel: None,
            shell_timeout_secs: 120,
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

    #[test]
    fn edit_file_rejects_oversized_file() {
        // edit_file loads the whole file into memory (replace semantics); a
        // file over EDIT_FILE_MAX_BYTES must be rejected before read_to_string
        // (OOM guard). A sparse file via set_len reports a large len() without
        // actually allocating the bytes; the capped take() read then streams a
        // hole of zeros, so this test stays cheap either way.
        let dir = std::env::temp_dir().join(format!("ta-edit-big-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.txt");
        {
            let f = std::fs::File::create(&big).unwrap();
            f.set_len(EDIT_FILE_MAX_BYTES + 1).unwrap(); // sparse hole, not 8 MB of data
        }
        let err = EditFile
            .run(
                &json!({"path": "big.txt", "old_string": "x", "new_string": "y"}),
                &ctx_in(&dir),
            )
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("over the"), "{msg}");
        assert!(msg.contains("limit"), "{msg}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_dir_overflow_marker_sorts_last() {
        // Over MAX_LIST_ENTRIES the listing carries a "... [more than N
        // entries]" marker. It must sit at the END of the sorted output —
        // pushed before the sort it byte-sorts as "..." ahead of most file
        // names and ends up at the top, masquerading as a directory entry.
        let dir = std::env::temp_dir().join(format!("ta-list-big-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for i in 0..MAX_LIST_ENTRIES + 5 {
            fs::write(dir.join(format!("f{i:04}")), "").unwrap();
        }
        let mut ctx = ctx_in(&dir);
        ctx.max_output_chars = 1_000_000; // don't truncate the marker away
        let out = ListDir.run(&json!({}), &ctx).unwrap();
        let marker = format!("... [more than {MAX_LIST_ENTRIES} entries]");
        let tail = out.content.rsplit('\n').next().unwrap();
        assert_eq!(tail, marker, "marker must be the last line: {tail:?}");
        // And exactly MAX_LIST_ENTRIES real entries precede it.
        assert_eq!(out.content.lines().count(), MAX_LIST_ENTRIES + 1);
        fs::remove_dir_all(&dir).ok();
    }

    /// restrict_to_workdir must refuse a path that escapes via a symlink inside
    /// the workdir. The whole point of confine()'s canonicalize step (vs a plain
    /// lexical prefix check) is to resolve symlinks, yet nothing in the suite
    /// exercised it — the existing escape test uses an absolute path, which a
    /// lexical check would also catch. This pins the symlink-resolution path so
    /// a future simplification can't silently drop it.
    #[cfg(unix)]
    #[test]
    fn write_file_rejects_symlink_escape_under_restrict() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("ta-sym-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // A target OUTSIDE the workdir, and a symlink inside it pointing there.
        let outside = std::env::temp_dir().join(format!("ta-sym-out-{}.txt", std::process::id()));
        fs::write(&outside, "orig").unwrap();
        symlink(&outside, dir.join("evil")).unwrap();
        // Writing through the symlink must be rejected before any I/O, and the
        // outside file must stay untouched.
        let res = WriteFile.run(
            &json!({"path": "evil", "content": "hacked"}),
            &ctx_restricted(&dir),
        );
        assert!(res.is_err(), "symlink escape must be rejected under restrict_to_workdir");
        assert_eq!(fs::read_to_string(&outside).unwrap(), "orig");
        fs::remove_dir_all(&dir).ok();
        let _ = fs::remove_file(&outside);
    }
}
