//! Skill discovery + loading — the "progressive disclosure" model from Claude
//! Code Agent Skills, adapted to TinyAgent's sync, config-driven architecture.
//!
//! A skill is a **subdirectory** of a skills dir that contains a `SKILL.md`.
//! The `SKILL.md` starts with a small YAML frontmatter (`name` + `description`);
//! the body is the full instruction set, and the dir may hold sidecar files the
//! body references (`reference.md`, `forms.md`, …). Three disclosure levels:
//!
//! 1. At agent assembly we scan the skills dirs (global
//!    `~/.config/tinyagent/skills` + project `<workdir>/.tinyagent/skills`, the
//!    latter overriding the former on a name clash) and preload **only** each
//!    skill's `name`+`description` into the system prompt — enough for the model
//!    to know when a skill is relevant, cheap to carry permanently.
//! 2. The model pulls a skill's full `SKILL.md` into context on demand via the
//!    [`crate::tools::skill::ReadSkill`] tool — arriving as a tool_result in
//!    history (compressible), not a permanent system addition.
//! 3. Sidecar files are read the same way, with a `file` argument.
//!
//! The two-field frontmatter is hand-parsed here: the project stays a minimal
//! single binary with no YAML dependency, and `name`/`description` are all the
//! system prompt needs. See the Anthropic engineering post on Agent Skills for
//! the rationale (metadata preloaded; body read into context only when used).

use std::path::{Path, PathBuf};

/// One discovered skill: its declared identity + the directory holding
/// `SKILL.md` (and any sidecar files).
#[derive(Clone, Debug)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
}

/// Discovered skills, queryable by name and sorted by name for a stable,
/// filesystem-order-independent listing in the system prompt. A project-dir
/// skill overrides a global one on a name clash (project wins).
#[derive(Clone, Debug, Default)]
pub struct SkillIndex {
    entries: Vec<SkillEntry>,
}

impl SkillIndex {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[SkillEntry] {
        &self.entries
    }

    /// Look up a skill's directory by name (linear scan — the index is small).
    pub fn dir_of(&self, name: &str) -> Option<&Path> {
        self.entries.iter().find(|e| e.name == name).map(|e| e.dir.as_path())
    }
}

/// Global skills dir: mirrors `main::default_config_path_in` (hand-rolled XDG,
/// no `dirs` crate) but joins `tinyagent/skills` instead of `tinyagent/config.json`.
pub fn default_global_skills_dir() -> Option<PathBuf> {
    default_global_skills_dir_in(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

fn default_global_skills_dir_in(xdg: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let base = match xdg {
        Some(x) if !x.is_empty() && Path::new(x).is_absolute() => PathBuf::from(x),
        _ => home
            .filter(|h| !h.is_empty())
            .map(|h| PathBuf::from(h).join(".config"))?,
    };
    Some(base.join("tinyagent").join("skills"))
}

/// Scan the given dirs (pass global first, project last) and collect valid
/// skills. Missing dirs are silently skipped — normal when no skills are
/// installed. Malformed skills (no `SKILL.md`, missing frontmatter fields,
/// illegal name) are skipped and reported via `warnings` rather than aborting
/// assembly: one bad skill must not break agent startup.
pub fn scan(dirs: &[PathBuf]) -> (SkillIndex, Vec<String>) {
    let mut index = SkillIndex::default();
    let mut warnings = Vec::new();
    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(_) => continue, // missing dir is normal — no skills installed there
        };
        for e in entries.flatten() {
            let sub = e.path();
            // A skill is a subdirectory containing SKILL.md; loose files are ignored.
            if !sub.is_dir() {
                continue;
            }
            let skill_md = sub.join("SKILL.md");
            let content = match std::fs::read_to_string(&skill_md) {
                Ok(c) => c,
                Err(_) => {
                    warnings.push(format!(
                        "skill dir {} has no readable SKILL.md, skipped",
                        sub.display()
                    ));
                    continue;
                }
            };
            let (name, description) = match parse_frontmatter(&content) {
                Some(v) => v,
                None => {
                    warnings.push(format!(
                        "skill dir {} SKILL.md missing name/description frontmatter, skipped",
                        sub.display()
                    ));
                    continue;
                }
            };
            if !valid_skill_name(&name) {
                warnings.push(format!(
                    "skill dir {} has invalid name {:?} (need [A-Za-z0-9_-]+), skipped",
                    sub.display(),
                    name
                ));
                continue;
            }
            upsert(
                &mut index.entries,
                SkillEntry { name, description, dir: sub.clone() },
            );
        }
    }
    // Deterministic order (independent of filesystem readdir order): sort by
    // name so the system-prompt intro list is stable and predictable.
    index.entries.sort_by(|a, b| a.name.cmp(&b.name));
    (index, warnings)
}

/// Insert or replace by name — a later dir wins on a clash (project overrides
/// global), preserving the first-seen position so the intro list stays stable.
fn upsert(entries: &mut Vec<SkillEntry>, e: SkillEntry) {
    if let Some(slot) = entries.iter_mut().find(|x| x.name == e.name) {
        *slot = e;
    } else {
        entries.push(e);
    }
}

/// Parse a two-field YAML frontmatter block (`name`, `description`) delimited
/// by `---` lines. Only single-line values are supported (multi-line YAML
/// folding is out of scope — the format asks for one-line descriptions).
/// Unknown keys are ignored. Returns `None` if there is no frontmatter or
/// either required field is missing/empty. Expects the conventional
/// `key: value` shape (no space before the colon).
fn parse_frontmatter(content: &str) -> Option<(String, String)> {
    let mut lines = content.lines();
    let first = lines.next()?;
    if first.trim() != "---" {
        return None;
    }
    let mut name = None;
    let mut description = None;
    for line in lines {
        let t = line.trim();
        if t == "---" {
            break; // end of frontmatter
        }
        if let Some(v) = strip_key(t, "name") {
            name = Some(v);
        } else if let Some(v) = strip_key(t, "description") {
            description = Some(v);
        }
        // other keys ignored
    }
    let name = name.filter(|n| !n.is_empty())?;
    let description = description.filter(|d| !d.is_empty())?;
    Some((name, description))
}

/// Match `key: value` (after trim), returning the value trimmed and with a
/// single pair of surrounding matching quotes stripped (`"` or `'`).
fn strip_key(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(&format!("{key}:"))?;
    Some(unquote(rest.trim()))
}

fn unquote(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() >= 2 && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// A skill name must be a safe directory component and a stable identifier
/// (same character set the MCP tool-name sanitizer allows).
fn valid_skill_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Render the level-1 intro appended to the system prompt. Lists each skill's
/// name + one-line description and tells the model how to load the full body
/// via `read_skill`. Kept deliberately short: the intro is permanent context
/// (compression never touches the system prompt) and counts toward the
/// compression-trigger estimate, so it carries name+description only — never
/// the body.
pub fn render_intro(prefix: Option<&str>, index: &SkillIndex) -> String {
    let mut out = String::new();
    if let Some(p) = prefix {
        if !p.is_empty() {
            out.push_str(p);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }
    }
    out.push_str("## 可用技能 (skills)\n");
    out.push_str(
        "需要时调用 read_skill 加载某个技能的完整指令 \
         (参数 {\"skill\": ...}, 可选 \"file\" 默认 SKILL.md, 可读同目录附属文件):\n",
    );
    for e in index.entries() {
        out.push_str(&format!("- {}: {}\n", e.name, e.description));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, name: &str, body: &str) {
        let sub = dir.join(name);
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("SKILL.md"), body).unwrap();
    }

    fn tmpd(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ta-skills-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn frontmatter_parses_name_and_description() {
        let c = "---\nname: deploy\ndescription: ship to prod\n---\nbody";
        assert_eq!(
            parse_frontmatter(c),
            Some(("deploy".into(), "ship to prod".into()))
        );
    }

    #[test]
    fn frontmatter_strips_surrounding_quotes() {
        let c = "---\nname: \"deploy\"\ndescription: 'ship it'\n---\n";
        assert_eq!(
            parse_frontmatter(c),
            Some(("deploy".into(), "ship it".into()))
        );
    }

    #[test]
    fn frontmatter_ignores_unknown_keys() {
        let c = "---\nname: x\ndescription: y\nversion: 1.2\nauthor: me\n---\n";
        assert_eq!(parse_frontmatter(c), Some(("x".into(), "y".into())));
    }

    #[test]
    fn frontmatter_description_may_contain_colons() {
        // Only the first `key:` is split; a colon inside the value is fine.
        let c = "---\nname: deploy\ndescription: deploy: staging then prod\n---\n";
        assert_eq!(
            parse_frontmatter(c),
            Some(("deploy".into(), "deploy: staging then prod".into()))
        );
    }

    #[test]
    fn frontmatter_missing_returns_none() {
        assert_eq!(parse_frontmatter("no frontmatter here"), None);
        assert_eq!(parse_frontmatter("---\ndescription: no name\n---\n"), None);
        assert_eq!(parse_frontmatter("---\nname: no desc\n---\n"), None);
    }

    #[test]
    fn valid_skill_name_rules() {
        assert!(valid_skill_name("code-review"));
        assert!(valid_skill_name("deploy_v2"));
        assert!(valid_skill_name("a"));
        assert!(!valid_skill_name(""));
        assert!(!valid_skill_name("has space"));
        assert!(!valid_skill_name("slash/here"));
        assert!(!valid_skill_name("dot."));
    }

    #[test]
    fn scan_collects_valid_skills() {
        let dir = tmpd("collect");
        write_skill(&dir, "alpha", "---\nname: alpha\ndescription: a\n---\nA");
        write_skill(&dir, "beta", "---\nname: beta\ndescription: b\n---\nB");
        let (idx, warns) = scan(std::slice::from_ref(&dir));
        assert!(warns.is_empty(), "{warns:?}");
        let names: Vec<&str> = idx.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(idx.dir_of("alpha").unwrap().file_name().unwrap(), "alpha");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_skips_bad_skills_with_warnings() {
        let dir = tmpd("bad");
        // good
        write_skill(&dir, "good", "---\nname: good\ndescription: g\n---\n");
        // no SKILL.md
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        // missing description
        write_skill(&dir, "nodesc", "---\nname: nodesc\n---\n");
        // illegal name
        write_skill(&dir, "ignored", "---\nname: bad name\ndescription: x\n---\n");
        let (idx, warns) = scan(std::slice::from_ref(&dir));
        let names: Vec<&str> = idx.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["good"]); // only the valid one
        assert_eq!(warns.len(), 3, "{warns:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_project_overrides_global_on_clash() {
        let global = tmpd("g");
        let project = tmpd("p");
        write_skill(&global, "shared", "---\nname: shared\ndescription: GLOBAL\n---\n");
        write_skill(&project, "shared", "---\nname: shared\ndescription: PROJECT\n---\n");
        write_skill(&global, "only-g", "---\nname: only-g\ndescription: g\n---\n");
        // global first, project last → project wins the clash, keeps first-seen order.
        let (idx, _) = scan(&[global.clone(), project.clone()]);
        let shared = idx.entries().iter().find(|e| e.name == "shared").unwrap();
        assert_eq!(shared.description, "PROJECT");
        assert!(shared.dir.starts_with(&project));
        // order is sorted by name (independent of readdir order)
        let names: Vec<&str> = idx.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["only-g", "shared"]);
        std::fs::remove_dir_all(&global).ok();
        std::fs::remove_dir_all(&project).ok();
    }

    #[test]
    fn scan_silently_skips_missing_dir() {
        let (idx, warns) = scan(&[PathBuf::from("/nonexistent/ta-skill-dir-xyz")]);
        assert!(idx.is_empty());
        assert!(warns.is_empty()); // missing dir is normal, not a warning
    }

    #[test]
    fn render_intro_appends_to_existing_prompt() {
        let idx = SkillIndex {
            entries: vec![SkillEntry {
                name: "deploy".into(),
                description: "ship it".into(),
                dir: PathBuf::from("/x/deploy"),
            }],
        };
        let out = render_intro(Some("you are an agent"), &idx);
        assert!(out.starts_with("you are an agent\n\n## 可用技能"));
        assert!(out.contains("- deploy: ship it"));
        assert!(out.contains("read_skill"));
    }

    #[test]
    fn render_intro_without_prefix() {
        let idx = SkillIndex {
            entries: vec![SkillEntry {
                name: "x".into(),
                description: "y".into(),
                dir: PathBuf::from("/x"),
            }],
        };
        let out = render_intro(None, &idx);
        assert!(out.starts_with("## 可用技能"));
    }

    #[test]
    fn global_dir_follows_xdg_then_home() {
        assert_eq!(
            default_global_skills_dir_in(Some("/custom/xdg"), None),
            Some(PathBuf::from("/custom/xdg/tinyagent/skills"))
        );
        assert_eq!(
            default_global_skills_dir_in(None, Some("/home/me")),
            Some(PathBuf::from("/home/me/.config/tinyagent/skills"))
        );
        // empty xdg falls back to home
        assert_eq!(
            default_global_skills_dir_in(Some(""), Some("/home/me")),
            Some(PathBuf::from("/home/me/.config/tinyagent/skills"))
        );
        // non-absolute xdg is ignored
        assert_eq!(
            default_global_skills_dir_in(Some("rel/path"), Some("/home/me")),
            Some(PathBuf::from("/home/me/.config/tinyagent/skills"))
        );
        assert_eq!(default_global_skills_dir_in(None, None), None);
    }
}
