use super::file::MAX_LIST_ENTRIES;
use super::{confined_to, truncate, Tool, ToolCtx, ToolOutput};
use crate::provider::ToolDef;
use crate::skills::SkillIndex;
use anyhow::Context;
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;

/// Load a skill's full `SKILL.md` (or a bundled resource file from a
/// subdirectory like `scripts/`, `references/`, `assets/`) into context on
/// demand — levels 2 and 3 of the agentskills.io progressive-disclosure model.
///
/// Each skill's `name` + one-line `description` already lives in the system
/// prompt (level 1, preloaded at assembly). This tool is how the model pulls in
/// the full instruction body only when relevant — and it arrives as a
/// `tool_result` in history (so compression can reclaim it), not as a permanent
/// system-prompt addition. Resource files under the skill's conventional
/// subdirectories are reached via a relative `file` path
/// (e.g. `"scripts/extract.py"`).
///
/// Holds the [`SkillIndex`] built at assembly. **Not** registered in the config
/// tool whitelist (`config::default_tools`): it is paired with the skill system
/// and injected automatically alongside the level-1 intro when skills are
/// discovered. Sub-agents build their own tool list from the whitelist, so they
/// don't carry it — matching the "skills belong to the main agent" decision.
pub struct ReadSkill {
    index: SkillIndex,
}

impl ReadSkill {
    pub fn new(index: SkillIndex) -> Self {
        Self { index }
    }
}

impl Tool for ReadSkill {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "read_skill".into(),
            description: "Load a skill's full instructions from its SKILL.md, or a \
                          bundled resource file (e.g. references/REFERENCE.md, \
                          scripts/extract.py). Skills and their one-line descriptions \
                          are listed in the system prompt under '可用技能'; call this \
                          only for a skill relevant to the current task."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "skill": {
                        "type": "string",
                        "description": "Skill name, as listed in the system prompt"
                    },
                    "file": {
                        "type": "string",
                        "description": "File within the skill directory to read (default SKILL.md). \
                                        A relative path may name a resource in a subdirectory, e.g. \
                                        'scripts/foo.sh' or 'references/REF.md'."
                    }
                },
                "required": ["skill"]
            }),
        }
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> anyhow::Result<ToolOutput> {
        let skill = input["skill"]
            .as_str()
            .context("read_skill: missing \"skill\"")?;
        let file = input["file"].as_str().unwrap_or("SKILL.md");
        // Two-layer confinement:
        // 1. `safe_relative_path` — a syntactic first pass that rejects obvious
        //    escapes (absolute paths, backslashes, `..`, empty/dot components)
        //    with a clear message before touching the filesystem.
        // 2. `confined_to` — the real boundary: canonicalize the skill dir and
        //    the deepest existing ancestor of the target, then assert the
        //    target stays under the skill dir (catches symlink escapes too).
        // Same best-effort TOCTOU caveat as the workdir sandbox.
        if !safe_relative_path(file) {
            return Ok(ToolOutput::error(format!(
                "read_skill: unsafe file path {file:?} — must be a relative path inside the skill directory (no .., absolute paths, or backslashes)"
            )));
        }
        let dir = match self.index.dir_of(skill) {
            Some(d) => d,
            None => {
                let known: Vec<&str> =
                    self.index.entries().iter().map(|e| e.name.as_str()).collect();
                return Ok(ToolOutput::error(format!(
                    "read_skill: unknown skill {skill:?}; available: {}",
                    known.join(", ")
                )));
            }
        };
        let full = dir.join(file);
        let resolved = match confined_to(dir, &full, "skill directory") {
            Ok(p) => p,
            Err(_) => {
                return Ok(ToolOutput::error(format!(
                    "read_skill: file {file:?} is outside the skill directory"
                )));
            }
        };
        // Bound the BYTES read from disk (same formula as read_file: 4× the
        // char cap + slack) — `read_to_string` would pull a huge SKILL.md
        // (or a runaway references/ file) fully into memory before the char
        // truncation below could throw it away. Strict UTF-8 semantics kept:
        // invalid content is an error, not silent lossy replacement.
        let max_bytes = (ctx.max_output_chars.saturating_mul(4)).saturating_add(1024) as u64;
        let mut bytes = Vec::new();
        std::fs::File::open(&resolved)
            .and_then(|f| f.take(max_bytes).read_to_end(&mut bytes))
            .with_context(|| format!("read_skill: cannot read {}", resolved.display()))?;
        let mut content = String::from_utf8(bytes)
            .with_context(|| format!("read_skill: {} is not valid UTF-8", resolved.display()))?;
        truncate(&mut content, ctx.max_output_chars);
        Ok(ToolOutput::text(content))
    }
}

/// List every file in a skill's directory tree (including the conventional
/// `scripts/`/`references/`/`assets/` subdirectories), as relative paths from
/// the skill root — level-3 resource *discovery*. The model uses this to see
/// what a skill bundles before pulling specific files with [`ReadSkill`].
///
/// Walks the skill directory itself (no user-supplied path), bounded in depth,
/// so there is no escape surface; symlinks are listed as entries but not
/// descended into (so a symlink pointing outside can't leak the target tree).
pub struct ListSkill {
    index: SkillIndex,
}

impl ListSkill {
    pub fn new(index: SkillIndex) -> Self {
        Self { index }
    }
}

impl Tool for ListSkill {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "list_skill".into(),
            description: "List all files in a skill's directory (including subdirectories \
                          like scripts/, references/, assets/) as relative paths from the \
                          skill root. Use this to discover a skill's bundled resources before \
                          reading them with read_skill. Skills are listed in the system \
                          prompt under '可用技能'."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "skill": {
                        "type": "string",
                        "description": "Skill name, as listed in the system prompt"
                    }
                },
                "required": ["skill"]
            }),
        }
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> anyhow::Result<ToolOutput> {
        let skill = input["skill"]
            .as_str()
            .context("list_skill: missing \"skill\"")?;
        let dir = match self.index.dir_of(skill) {
            Some(d) => d,
            None => {
                let known: Vec<&str> =
                    self.index.entries().iter().map(|e| e.name.as_str()).collect();
                return Ok(ToolOutput::error(format!(
                    "list_skill: unknown skill {skill:?}; available: {}",
                    known.join(", ")
                )));
            }
        };
        const MAX_DEPTH: usize = 4;
        let mut paths: Vec<String> = Vec::new();
        walk_skill(dir, dir, 0, MAX_DEPTH, &mut paths);
        paths.sort();
        let mut out = if paths.is_empty() {
            String::from("(skill directory is empty)")
        } else {
            paths.join("\n")
        };
        truncate(&mut out, ctx.max_output_chars);
        Ok(ToolOutput::text(out))
    }
}

/// Recursively list files under `root` as paths relative to `root`, up to
/// `max_depth` levels deep. Hidden entries (leading `.`) are skipped. Uses
/// `file_type` (which does NOT follow symlinks) to decide whether to descend,
/// so a symlinked directory is reported as an entry but not walked. Each
/// directory enumerates at most [`MAX_LIST_ENTRIES`] entries (matching
/// list_dir): a runaway generated dir can't balloon the listing past the
/// truncation budget — instead the directory gets an explicit truncation
/// marker so the model KNOWS the listing is partial.
fn walk_skill(root: &Path, cur: &Path, depth: usize, max_depth: usize, out: &mut Vec<String>) {
    let rd = match std::fs::read_dir(cur) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut seen = 0usize;
    for e in rd.flatten() {
        seen += 1;
        if seen > MAX_LIST_ENTRIES {
            out.push("... [directory listing truncated]".into());
            break;
        }
        if e.file_name().to_string_lossy().starts_with('.') {
            continue; // skip hidden
        }
        let path = e.path();
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().into_owned(),
            Err(_) => continue,
        };
        out.push(rel);
        let descend = e
            .file_type()
            .map(|ft| ft.is_dir() && depth + 1 < max_depth)
            .unwrap_or(false);
        if descend {
            walk_skill(root, &path, depth + 1, max_depth, out);
        }
    }
}

/// A safe relative path inside the skill directory: non-empty, no backslash,
/// no `:` (Windows drive prefix / NTFS alternate data stream — and legal
/// skill resource names never carry one), not absolute (no leading `/`), and
/// no empty/`.`/`..` component. Allows `/` as a subdirectory separator
/// (e.g. `scripts/foo.sh`). This is the syntactic first layer of confinement;
/// `confined_to` is the canonicalize-based second.
fn safe_relative_path(s: &str) -> bool {
    if s.is_empty() || s.starts_with('/') || s.contains('\\') || s.contains(':') {
        return false;
    }
    for comp in s.split('/') {
        if comp.is_empty() || comp == "." || comp == ".." {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    /// Drop valid skills named `names` into `dir`, each with a parseable
    /// SKILL.md frontmatter, and build the index the real way (via `scan`) so
    /// the test exercises the same path assembly uses and stays off
    /// SkillIndex's private fields.
    fn index_in(dir: &std::path::Path, names: &[&str]) -> SkillIndex {
        for n in names {
            let sub = dir.join(n);
            fs::create_dir_all(&sub).unwrap();
            fs::write(
                sub.join("SKILL.md"),
                format!("---\nname: {n}\ndescription: skill {n}\n---\nbody-{n}\n"),
            )
            .unwrap();
        }
        let (index, warnings) = crate::skills::scan(&[dir.to_path_buf()]);
        assert!(warnings.is_empty(), "unexpected scan warnings: {warnings:?}");
        index
    }

    fn ctx(dir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            workdir: dir.to_path_buf(),
            max_output_chars: 1000,
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
    fn safe_relative_path_rules() {
        assert!(safe_relative_path("SKILL.md"));
        assert!(safe_relative_path("scripts/foo.sh"));
        assert!(safe_relative_path("references/REF.md"));
        assert!(safe_relative_path("a/b/c.json"));
        assert!(!safe_relative_path(""));
        assert!(!safe_relative_path("../secret"));
        assert!(!safe_relative_path("a/../b"));
        assert!(!safe_relative_path("/etc/passwd"));
        assert!(!safe_relative_path("a\\b"));
        assert!(!safe_relative_path("a//b"));
        assert!(!safe_relative_path("a/."));
        assert!(!safe_relative_path("."));
        assert!(!safe_relative_path(".."));
        // `:` is rejected unconditionally: Windows drive prefix, NTFS
        // alternate data stream. Legal skill resource names never carry one.
        assert!(!safe_relative_path("C:foo"));
        assert!(!safe_relative_path("C:/foo"));
        assert!(!safe_relative_path("file:stream"));
    }

    #[test]
    fn reads_skill_md_by_default() {
        let dir = std::env::temp_dir().join(format!("ta-rs-default-{}", std::process::id()));
        let index = index_in(&dir, &["deploy"]);
        let out = ReadSkill::new(index)
            .run(&json!({"skill": "deploy"}), &ctx(&dir))
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("body-deploy"), "{}", out.content);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_named_sidecar_file() {
        let dir = std::env::temp_dir().join(format!("ta-rs-sidecar-{}", std::process::id()));
        let index = index_in(&dir, &["deploy"]);
        fs::write(dir.join("deploy").join("reference.md"), "REFERENCE TEXT").unwrap();
        let out = ReadSkill::new(index)
            .run(&json!({"skill": "deploy", "file": "reference.md"}), &ctx(&dir))
            .unwrap();
        assert_eq!(out.content, "REFERENCE TEXT");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_nested_subdir_file() {
        let dir = std::env::temp_dir().join(format!("ta-rs-nested-{}", std::process::id()));
        let index = index_in(&dir, &["deploy"]);
        fs::create_dir_all(dir.join("deploy").join("scripts")).unwrap();
        fs::write(dir.join("deploy").join("scripts").join("release.sh"), "RELEASE").unwrap();
        fs::create_dir_all(dir.join("deploy").join("references")).unwrap();
        fs::write(
            dir.join("deploy").join("references").join("REFERENCE.md"),
            "REF",
        )
        .unwrap();
        let tool = ReadSkill::new(index);
        let out = tool
            .run(&json!({"skill": "deploy", "file": "scripts/release.sh"}), &ctx(&dir))
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "RELEASE");
        let out = tool
            .run(&json!({"skill": "deploy", "file": "references/REFERENCE.md"}), &ctx(&dir))
            .unwrap();
        assert_eq!(out.content, "REF");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_traversal_and_separators_in_file() {
        let dir = std::env::temp_dir().join(format!("ta-rs-escape-{}", std::process::id()));
        // Drop a secret OUTSIDE the skill dir; a traversal `file` must not reach it.
        let secret = dir.join("secret.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&secret, "TOPSECRET").unwrap();
        let index = index_in(&dir, &["deploy"]);
        let tool = ReadSkill::new(index);
        // `a/b` is intentionally NOT in this list — it is now a valid relative
        // path to a (possibly nested) resource; see reads_nested_subdir_file.
        for bad in ["../secret.txt", "..", "/etc/passwd", "sub\\x"] {
            let out = tool
                .run(&json!({"skill": "deploy", "file": bad}), &ctx(&dir))
                .unwrap();
            assert!(out.is_error, "{bad:?} should be rejected: {}", out.content);
            assert!(out.content.contains("unsafe file path"), "{bad:?}: {}", out.content);
        }
        // The secret was never read.
        assert_eq!(fs::read_to_string(&secret).unwrap(), "TOPSECRET");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_skill_unknown_lists_available() {
        let dir = std::env::temp_dir().join(format!("ta-rs-unknown-{}", std::process::id()));
        let index = index_in(&dir, &["alpha", "beta"]);
        let out = ReadSkill::new(index)
            .run(&json!({"skill": "nope"}), &ctx(&dir))
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("unknown skill"), "{}", out.content);
        assert!(out.content.contains("alpha") && out.content.contains("beta"), "{}", out.content);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncates_oversized_content() {
        let dir = std::env::temp_dir().join(format!("ta-rs-trunc-{}", std::process::id()));
        let sub = dir.join("big");
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            sub.join("SKILL.md"),
            format!("---\nname: big\ndescription: d\n---\n{}", "x".repeat(50_000)),
        )
        .unwrap();
        let (index, _) = crate::skills::scan(std::slice::from_ref(&dir));
        let mut c = ctx(&dir);
        c.max_output_chars = 500;
        let out = ReadSkill::new(index).run(&json!({"skill": "big"}), &c).unwrap();
        assert!(out.content.len() < 2000, "got {} bytes", out.content.len());
        assert!(out.content.contains("truncated"), "{}", out.content);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_skill_byte_cap_bounds_huge_files() {
        // B3: the DISK read is bounded (4×max_output_chars + 1024 bytes), not
        // just the post-read char truncation — a multi-MB SKILL.md comes back
        // truncated and is never pulled whole into memory. Single line is the
        // worst case for unbounded reads.
        let dir = std::env::temp_dir().join(format!("ta-rs-bytecap-{}", std::process::id()));
        let sub = dir.join("big");
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            sub.join("SKILL.md"),
            format!("---\nname: big\ndescription: d\n---\n{}", "y".repeat(3_000_000)),
        )
        .unwrap();
        let (index, _) = crate::skills::scan(std::slice::from_ref(&dir));
        let mut c = ctx(&dir);
        c.max_output_chars = 400;
        let out = ReadSkill::new(index).run(&json!({"skill": "big"}), &c).unwrap();
        assert!(out.content.len() < 1600, "got {} bytes", out.content.len());
        assert!(out.content.contains("truncated"), "{}", out.content);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_skill_lists_tree() {
        let dir = std::env::temp_dir().join(format!("ta-rs-list-{}", std::process::id()));
        let index = index_in(&dir, &["deploy"]);
        fs::create_dir_all(dir.join("deploy").join("scripts")).unwrap();
        fs::write(dir.join("deploy").join("scripts").join("release.sh"), "x").unwrap();
        fs::write(dir.join("deploy").join("references.md"), "y").unwrap();
        let out = ListSkill::new(index)
            .run(&json!({"skill": "deploy"}), &ctx(&dir))
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("SKILL.md"), "{}", out.content);
        // walk_skill emits paths with the platform separator (\ on Windows).
        let sep = if cfg!(unix) { "/" } else { "\\" };
        assert!(out.content.contains(&format!("scripts{sep}release.sh")), "{}", out.content);
        assert!(out.content.contains("references.md"), "{}", out.content);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_skill_unknown_lists_available() {
        let dir = std::env::temp_dir().join(format!("ta-rs-lu-{}", std::process::id()));
        let index = index_in(&dir, &["alpha"]);
        let out = ListSkill::new(index)
            .run(&json!({"skill": "nope"}), &ctx(&dir))
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("unknown skill"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_skill_truncates_runaway_directories() {
        // C12: a per-directory cap (500, matching list_dir) with an explicit
        // marker — a generated directory with thousands of entries must not
        // balloon the listing past the output budget silently; the model has
        // to KNOW the listing is partial.
        let dir = std::env::temp_dir().join(format!("ta-rs-cap-{}", std::process::id()));
        let index = index_in(&dir, &["gen"]);
        let sub = dir.join("gen").join("out");
        fs::create_dir_all(&sub).unwrap();
        for i in 0..600 {
            fs::write(sub.join(format!("f{i:03}.txt")), "x").unwrap();
        }
        let out = ListSkill::new(index)
            .run(&json!({"skill": "gen"}), &ctx(&dir))
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("[directory listing truncated]"),
            "marker missing: {}…",
            &out.content[..out.content.len().min(200)]
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_skill_skips_hidden_and_is_bounded() {
        let dir = std::env::temp_dir().join(format!("ta-rs-bnd-{}", std::process::id()));
        let index = index_in(&dir, &["deploy"]);
        fs::write(dir.join("deploy").join(".hidden"), "h").unwrap();
        // A leaf nested deeper than MAX_DEPTH (4) must not be listed.
        let mut deep = dir.join("deploy");
        for c in ["a", "b", "c", "d", "e"] {
            deep = deep.join(c);
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("leaf.txt"), "deep").unwrap();
        let out = ListSkill::new(index)
            .run(&json!({"skill": "deploy"}), &ctx(&dir))
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(!out.content.contains(".hidden"), "{}", out.content);
        assert!(!out.content.contains("leaf.txt"), "{}", out.content);
        fs::remove_dir_all(&dir).ok();
    }
}
