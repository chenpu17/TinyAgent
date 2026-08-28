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
//!    [`crate::tools::ReadSkill`] tool — arriving as a tool_result in
//!    history (compressible), not a permanent system addition.
//! 3. Sidecar files are read the same way, with a `file` argument.
//!
//! The two-field frontmatter is hand-parsed here: the project stays a minimal
//! single binary with no YAML dependency, and `name`/`description` are all the
//! system prompt needs. See the Anthropic engineering post on Agent Skills for
//! the rationale (metadata preloaded; body read into context only when used).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One discovered skill: its declared identity + the directory holding
/// `SKILL.md` (and any sidecar files). Carries the full agentskills.io
/// frontmatter — `license`/`compatibility`/`allowed_tools`/`metadata` are
/// parsed and stored so a standard skill loads without error, though only
/// `name`+`description` surface in the level-1 system-prompt intro (the intro
/// is permanent context and counts toward the compression estimate, so it must
/// stay short). `allowed_tools` additionally drives a whitelist check at
/// assembly. `Default` is derived so tests can build one with `..Default::default()`.
#[derive(Clone, Debug, Default)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    /// `allowed-tools` frontmatter, split on whitespace. Experimental per the
    /// spec; TinyAgent only warns at assembly (no per-turn activation layer).
    pub allowed_tools: Vec<String>,
    /// `metadata` frontmatter block (string→string). Accepted, not surfaced.
    pub metadata: HashMap<String, String>,
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
        self.entries
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.dir.as_path())
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
            let fm = match parse_frontmatter(&content) {
                Some(v) => v,
                None => {
                    warnings.push(format!(
                        "skill dir {} SKILL.md missing name/description frontmatter, skipped",
                        sub.display()
                    ));
                    continue;
                }
            };
            if !valid_skill_name(&fm.name) {
                warnings.push(format!(
                    "skill dir {} has invalid name {:?} (standard: 1-64 chars, [a-z0-9] only, \
                     no leading/trailing '-' or '--'), skipped",
                    sub.display(),
                    fm.name
                ));
                continue;
            }
            // agentskills.io: name must match the parent directory name.
            let dir_name = sub.file_name().and_then(|s| s.to_str());
            if dir_name != Some(fm.name.as_str()) {
                warnings.push(format!(
                    "skill dir {} frontmatter name {:?} must match the directory name, skipped",
                    sub.display(),
                    fm.name
                ));
                continue;
            }
            // Count CHARS, not bytes: the standard caps description length at
            // 1024 characters, and a CJK description triples in UTF-8 — a
            // byte count wrongly skipped perfectly legal skills.
            if fm.description.chars().count() > 1024 {
                warnings.push(format!(
                    "skill dir {} description exceeds 1024 chars ({}, standard max), skipped",
                    sub.display(),
                    fm.description.chars().count()
                ));
                continue;
            }
            upsert(
                &mut index.entries,
                SkillEntry {
                    name: fm.name,
                    description: fm.description,
                    dir: sub.clone(),
                    license: fm.license,
                    compatibility: fm.compatibility,
                    allowed_tools: fm.allowed_tools,
                    metadata: fm.metadata,
                },
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

/// Parsed agentskills.io frontmatter — all six standard fields. `scan` consumes
/// this, applies the name/description validation rules, and assembles a
/// [`SkillEntry`]. Kept separate from `SkillEntry` so validation stays out of
/// the parser.
struct ParsedFrontmatter {
    name: String,
    description: String,
    license: Option<String>,
    compatibility: Option<String>,
    allowed_tools: Vec<String>,
    metadata: HashMap<String, String>,
}

/// Parse the agentskills.io YAML frontmatter block delimited by `---` lines.
///
/// All six standard fields are recognized: `name`/`description` (required),
/// `license`/`compatibility` (optional single-line), `allowed-tools` (optional,
/// whitespace-split into a list), and `metadata` (optional indented `key:
/// value` block → map). Only single-line scalar values are supported for the
/// scalar fields; `metadata` is the one block form and is collected by a small
/// indent-driven state machine (no nested YAML). Unknown keys are ignored —
/// the open spec permits them (only Claude Code's upload path rejects unknown
/// keys). Returns `None` if there is no frontmatter or either required field is
/// missing/empty. Expects the conventional `key: value` shape (no space before
/// the colon).
fn parse_frontmatter(content: &str) -> Option<ParsedFrontmatter> {
    // Strip a UTF-8 BOM if present: Windows editors (VS Code default, PowerShell
    // `>` redirection) commonly emit one, and U+FEFF is NOT whitespace
    // (char::is_whitespace returns false for it), so trim() below wouldn't
    // remove it — the first line would read `\u{feff}---` and fail the `== "---"`
    // check, silently skipping the whole skill with a misleading "missing
    // frontmatter" warning.
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut lines = content.lines();
    let first = lines.next()?;
    if first.trim() != "---" {
        return None;
    }
    let mut name = None;
    let mut description = None;
    let mut license = None;
    let mut compatibility = None;
    let mut allowed_tools: Vec<String> = Vec::new();
    let mut metadata: HashMap<String, String> = HashMap::new();
    let mut in_metadata = false;
    for line in lines {
        let t = line.trim();
        if t == "---" {
            break; // end of frontmatter
        }
        if t.is_empty() {
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        // Inside a `metadata:` block: collect indented `key: value` lines until
        // a non-indented line ends the block.
        if in_metadata {
            if indented {
                if let Some((k, v)) = split_kv(t) {
                    metadata.insert(k.to_string(), unquote(v));
                }
                continue;
            }
            in_metadata = false;
        }
        if let Some(v) = strip_key(t, "name") {
            name = Some(v);
        } else if let Some(v) = strip_key(t, "description") {
            description = Some(v);
        } else if let Some(v) = strip_key(t, "license") {
            license = Some(v);
        } else if let Some(v) = strip_key(t, "compatibility") {
            compatibility = Some(v);
        } else if let Some(v) = strip_key(t, "allowed-tools") {
            allowed_tools = v.split_whitespace().map(String::from).collect();
        } else if strip_key(t, "metadata").is_some() {
            // `metadata:` introduces an indented block; any inline value is
            // ignored (the spec models metadata as a map, not a scalar).
            in_metadata = true;
        }
        // other keys ignored
    }
    let name = name.filter(|n| !n.is_empty())?;
    let description = description.filter(|d| !d.is_empty())?;
    Some(ParsedFrontmatter {
        name,
        description,
        license,
        compatibility,
        allowed_tools,
        metadata,
    })
}

/// Match `key: value` (after trim), returning the value trimmed and with a
/// single pair of surrounding matching quotes stripped (`"` or `'`).
fn strip_key(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(&format!("{key}:"))?;
    Some(unquote(rest.trim()))
}

/// Split a `key: value` line at the first colon (used for `metadata` entries,
/// whose keys are arbitrary). Returns `None` if there is no colon or the key is
/// empty.
fn split_kv(s: &str) -> Option<(&str, &str)> {
    let idx = s.find(':')?;
    let k = s[..idx].trim();
    let v = s[idx + 1..].trim();
    if k.is_empty() {
        return None;
    }
    Some((k, v))
}

fn unquote(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() >= 2
        && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// A skill name per the agentskills.io spec: 1-64 chars, only lowercase letters,
/// digits, and hyphens; must not start or end with a hyphen or contain `--`.
/// (All allowed chars are ASCII one-byte, so byte length == char count.)
fn valid_skill_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
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
         (参数 {\"skill\": ...}, 可选 \"file\" 默认 SKILL.md, 支持相对路径读 scripts/ 等子目录附属文件);\
         先用 list_skill 列出某技能目录下的全部文件以发现资源:\n",
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

    fn fm(content: &str) -> ParsedFrontmatter {
        parse_frontmatter(content).expect("frontmatter should parse")
    }

    #[test]
    fn frontmatter_parses_name_and_description() {
        let f = fm("---\nname: deploy\ndescription: ship to prod\n---\nbody");
        assert_eq!(f.name, "deploy");
        assert_eq!(f.description, "ship to prod");
        // optional fields default when absent
        assert!(f.license.is_none());
        assert!(f.compatibility.is_none());
        assert!(f.allowed_tools.is_empty());
        assert!(f.metadata.is_empty());
    }

    #[test]
    fn frontmatter_strips_surrounding_quotes() {
        let f = fm("---\nname: \"deploy\"\ndescription: 'ship it'\n---\n");
        assert_eq!(f.name, "deploy");
        assert_eq!(f.description, "ship it");
    }

    #[test]
    fn frontmatter_strips_utf8_bom() {
        // Windows editors (VS Code default, PowerShell redirection) commonly
        // save SKILL.md with a UTF-8 BOM (U+FEFF). U+FEFF is NOT whitespace, so
        // the first line becomes "\u{feff}---" and the frontmatter check fails
        // — silently skipping the skill with a misleading "missing frontmatter"
        // warning. parse_frontmatter must strip the BOM before checking.
        let bom = "\u{feff}";
        let f = fm(&format!(
            "{bom}---\nname: deploy\ndescription: ship\n---\nbody"
        ));
        assert_eq!(f.name, "deploy");
        assert_eq!(f.description, "ship");
    }

    #[test]
    fn frontmatter_parses_all_optional_fields() {
        let c = "---\n\
                 name: pdf-forms\n\
                 description: extract form fields\n\
                 license: Apache-2.0\n\
                 compatibility: needs python3\n\
                 allowed-tools: read_file shell list_dir\n\
                 ---\n";
        let f = fm(c);
        assert_eq!(f.license.as_deref(), Some("Apache-2.0"));
        assert_eq!(f.compatibility.as_deref(), Some("needs python3"));
        assert_eq!(f.allowed_tools, vec!["read_file", "shell", "list_dir"]);
    }

    #[test]
    fn frontmatter_parses_metadata_block() {
        // Note: written on one line with explicit \n and REAL leading spaces —
        // Rust's `\` line-continuation would strip the very indentation the
        // metadata-block parser keys off.
        let c = "---\nname: x\ndescription: y\nmetadata:\n  author: alice\n  version: '2'\n---\n";
        let f = fm(c);
        assert_eq!(f.metadata.get("author").map(String::as_str), Some("alice"));
        assert_eq!(f.metadata.get("version").map(String::as_str), Some("2"));
    }

    #[test]
    fn frontmatter_metadata_block_ends_at_nonindented_line() {
        // A top-level key after the indented metadata block is parsed normally.
        let c = "---\nname: x\ndescription: y\nmetadata:\n  a: 1\nlicense: MIT\n---\n";
        let f = fm(c);
        assert_eq!(f.metadata.get("a").map(String::as_str), Some("1"));
        assert_eq!(f.license.as_deref(), Some("MIT"));
    }

    #[test]
    fn frontmatter_ignores_unknown_keys() {
        let f = fm("---\nname: x\ndescription: y\nversion: 1.2\nauthor: me\n---\n");
        assert_eq!(f.name, "x");
        assert_eq!(f.description, "y");
    }

    #[test]
    fn frontmatter_description_may_contain_colons() {
        // Only the first `key:` is split; a colon inside the value is fine.
        let f = fm("---\nname: deploy\ndescription: deploy: staging then prod\n---\n");
        assert_eq!(f.description, "deploy: staging then prod");
    }

    #[test]
    fn frontmatter_missing_returns_none() {
        assert!(parse_frontmatter("no frontmatter here").is_none());
        assert!(parse_frontmatter("---\ndescription: no name\n---\n").is_none());
        assert!(parse_frontmatter("---\nname: no desc\n---\n").is_none());
    }

    #[test]
    fn valid_skill_name_rules() {
        assert!(valid_skill_name("code-review"));
        assert!(valid_skill_name("deploy"));
        assert!(valid_skill_name("a"));
        assert!(valid_skill_name("pdf-form-extraction"));
        // standard rejections
        assert!(!valid_skill_name(""));
        assert!(!valid_skill_name("Deploy")); // uppercase not allowed
        assert!(!valid_skill_name("deploy_v2")); // underscore not allowed
        assert!(!valid_skill_name("-leading"));
        assert!(!valid_skill_name("trailing-"));
        assert!(!valid_skill_name("double--hyphen"));
        assert!(!valid_skill_name(&"x".repeat(65))); // >64 chars
        assert!(!valid_skill_name("has space"));
        assert!(!valid_skill_name("slash/here"));
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
        write_skill(
            &dir,
            "ignored",
            "---\nname: bad name\ndescription: x\n---\n",
        );
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
        write_skill(
            &global,
            "shared",
            "---\nname: shared\ndescription: GLOBAL\n---\n",
        );
        write_skill(
            &project,
            "shared",
            "---\nname: shared\ndescription: PROJECT\n---\n",
        );
        write_skill(
            &global,
            "only-g",
            "---\nname: only-g\ndescription: g\n---\n",
        );
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
    fn scan_rejects_name_not_matching_directory() {
        let dir = tmpd("namemismatch");
        // directory "ship-it" but frontmatter name "deploy" — spec: name must
        // match the parent directory name.
        write_skill(&dir, "ship-it", "---\nname: deploy\ndescription: x\n---\n");
        let (idx, warns) = scan(std::slice::from_ref(&dir));
        assert!(idx.is_empty(), "should be skipped");
        assert_eq!(warns.len(), 1, "{warns:?}");
        assert!(
            warns[0].contains("must match the directory name"),
            "{warns:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_rejects_oversized_description() {
        let dir = tmpd("bigdesc");
        let long = "x".repeat(1025);
        write_skill(
            &dir,
            "big",
            &format!("---\nname: big\ndescription: {long}\n---\n"),
        );
        let (idx, warns) = scan(std::slice::from_ref(&dir));
        assert!(idx.is_empty());
        assert_eq!(warns.len(), 1, "{warns:?}");
        assert!(warns[0].contains("exceeds 1024"), "{warns:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_counts_description_in_chars_not_bytes() {
        // C13: the 1024 cap is in CHARACTERS (agentskills.io). A CJK
        // description triples in UTF-8 — 600 汉字 = 1800 bytes < 1024 chars,
        // and the old byte count wrongly skipped it as oversized.
        let dir = tmpd("cjkdesc");
        let desc = "汉".repeat(600); // 600 chars, 1800 UTF-8 bytes
        assert_eq!(desc.len(), 1800);
        write_skill(
            &dir,
            "cjk",
            &format!("---\nname: cjk\ndescription: {desc}\n---\n"),
        );
        let (idx, warns) = scan(std::slice::from_ref(&dir));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(idx.entries().len(), 1);
        // …while a true char overrun (1025 chars of 汉字) is still rejected.
        let dir2 = tmpd("cjkdesc2");
        let over = "汉".repeat(1025);
        write_skill(
            &dir2,
            "cjk2",
            &format!("---\nname: cjk2\ndescription: {over}\n---\n"),
        );
        let (idx2, warns2) = scan(std::slice::from_ref(&dir2));
        assert!(idx2.is_empty());
        assert_eq!(warns2.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&dir2).ok();
    }

    #[test]
    fn scan_stores_optional_frontmatter_fields() {
        let dir = tmpd("optional");
        let sub = dir.join("rich");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join("SKILL.md"),
            "---\nname: rich\ndescription: d\nlicense: MIT\ncompatibility: py3\n\
             allowed-tools: shell read_file\nmetadata:\n  k: v\n---\n",
        )
        .unwrap();
        let (idx, warns) = scan(std::slice::from_ref(&dir));
        assert!(warns.is_empty(), "{warns:?}");
        let e = idx.entries().iter().find(|e| e.name == "rich").unwrap();
        assert_eq!(e.license.as_deref(), Some("MIT"));
        assert_eq!(e.compatibility.as_deref(), Some("py3"));
        assert_eq!(e.allowed_tools, vec!["shell", "read_file"]);
        assert_eq!(e.metadata.get("k").map(String::as_str), Some("v"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_intro_appends_to_existing_prompt() {
        let idx = SkillIndex {
            entries: vec![SkillEntry {
                name: "deploy".into(),
                description: "ship it".into(),
                dir: PathBuf::from("/x/deploy"),
                ..Default::default()
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
                ..Default::default()
            }],
        };
        let out = render_intro(None, &idx);
        assert!(out.starts_with("## 可用技能"));
    }

    #[test]
    fn global_dir_follows_xdg_then_home() {
        // is_absolute() is platform-dependent (Windows needs a drive letter),
        // so pass a host-absolute XDG path or the "xdg wins" branch isn't taken
        // on Windows. Build the expected path with join() (separator-agnostic).
        let xdg = if cfg!(unix) {
            "/custom/xdg"
        } else {
            "C:/custom/xdg"
        };
        assert_eq!(
            default_global_skills_dir_in(Some(xdg), None),
            Some(PathBuf::from(xdg).join("tinyagent").join("skills"))
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
