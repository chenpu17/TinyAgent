use super::{truncate, Tool, ToolCtx, ToolOutput};
use crate::provider::ToolDef;
use crate::skills::SkillIndex;
use anyhow::Context;
use serde_json::{json, Value};

/// Load a skill's full `SKILL.md` (or a bundled sidecar file) into context on
/// demand — levels 2 and 3 of the progressive-disclosure model.
///
/// Each skill's `name` + one-line `description` already lives in the system
/// prompt (level 1, preloaded at assembly). This tool is how the model pulls in
/// the full instruction body only when relevant — and it arrives as a
/// `tool_result` in history (so compression can reclaim it), not as a permanent
/// system-prompt addition.
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
                          bundled sidecar file (e.g. reference.md). Skills and their \
                          one-line descriptions are listed in the system prompt under \
                          '可用技能'; call this only for a skill relevant to the \
                          current task."
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
                        "description": "File within the skill directory to read (default SKILL.md)"
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
        // `file` must be a single component inside the skill dir: reject any
        // path separator or traversal so it can't escape (e.g. "../secret" or
        // "a/b"). The skill name is already validated at scan time and resolved
        // through the index, so it can't escape either.
        let file = input["file"].as_str().unwrap_or("SKILL.md");
        if !valid_file_name(file) {
            return Ok(ToolOutput::error(format!(
                "read_skill: unsafe file name {file:?} — must be a single filename \
                 component (no path separators or ..)"
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
        let mut content = std::fs::read_to_string(&full)
            .with_context(|| format!("read_skill: cannot read {}", full.display()))?;
        truncate(&mut content, ctx.max_output_chars);
        Ok(ToolOutput::text(content))
    }
}

/// A safe sidecar filename: non-empty, a single component (no `/` or `\`), not
/// `.`/`..`, and only `[A-Za-z0-9._-]`. This keeps `file` confined to the skill
/// directory — a first line of defense alongside the skill-name validation done
/// at scan time. Not a hard security boundary (same TOCTOU caveat as the file
/// sandbox): it keeps the model from straying, not a hostile process.
fn valid_file_name(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('/')
        && !s.contains('\\')
        && s != "."
        && s != ".."
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
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
        }
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
    fn rejects_traversal_and_separators_in_file() {
        let dir = std::env::temp_dir().join(format!("ta-rs-escape-{}", std::process::id()));
        // Drop a secret OUTSIDE the skill dir; a traversal `file` must not reach it.
        let secret = dir.join("secret.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&secret, "TOPSECRET").unwrap();
        let index = index_in(&dir, &["deploy"]);
        let tool = ReadSkill::new(index);
        for bad in ["../secret.txt", "a/b", "..", "/etc/passwd", "sub\\x"] {
            let out = tool
                .run(&json!({"skill": "deploy", "file": bad}), &ctx(&dir))
                .unwrap();
            assert!(out.is_error, "{bad:?} should be rejected: {}", out.content);
            assert!(out.content.contains("unsafe file name"), "{bad:?}: {}", out.content);
        }
        // The secret was never read.
        assert_eq!(fs::read_to_string(&secret).unwrap(), "TOPSECRET");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_skill_lists_available() {
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
}
