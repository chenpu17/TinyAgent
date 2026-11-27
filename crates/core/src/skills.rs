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

/// Where a skill's content lives: a real directory on disk (skills-dir scan,
/// with `SKILL.md` + optional sidecar files) or embedded in the binary (a
/// built-in skill — full `SKILL.md` text, no sidecars). Disk entries win on a
/// name clash: a user's `.tinyagent/skills/<name>/` overrides the built-in,
/// so the shipped guide is never a cage.
/// Core-produced (the loader constructs it; hosts never match on it) —
/// deliberately NOT `#[non_exhaustive]`, per the `AgentEvent` hygiene rule.
#[derive(Clone, Debug)]
pub enum SkillSource {
    /// A directory on disk (`<dir>/<name>/` containing `SKILL.md`). Sidecar
    /// files referenced by the body are readable through `read_skill`'s `file`
    /// argument.
    Dir(PathBuf),
    /// Full `SKILL.md` text (frontmatter + body), compiled into the binary.
    Embedded(String),
}

impl Default for SkillSource {
    fn default() -> Self {
        SkillSource::Dir(PathBuf::new())
    }
}

/// One discovered skill: its declared identity + where its content lives.
/// Carries the full agentskills.io frontmatter — `license`/`compatibility`/
/// `allowed_tools`/`metadata` are parsed and stored so a standard skill loads
/// without error, though only `name`+`description` surface in the level-1
/// system-prompt intro (the intro is permanent context and counts toward the
/// compression estimate, so it must stay short). `allowed_tools` additionally
/// drives a whitelist check at assembly. `Default` is derived so tests can
/// build one with `..Default::default()`.
#[derive(Clone, Debug, Default)]
pub struct SkillEntry {
    /// The `name` frontmatter value; must equal the parent directory name for
    /// disk skills (a mismatch is a scan warning, entry skipped).
    pub name: String,
    /// The `description` frontmatter value — the ONLY prose preloaded into
    /// the system prompt at level 1, so it is the model's sole clue that this
    /// skill exists.
    pub description: String,
    /// Where the body lives (disk dir or embedded text).
    pub source: SkillSource,
    /// `license` frontmatter, accepted and stored but not surfaced anywhere.
    pub license: Option<String>,
    /// `compatibility` frontmatter, accepted and stored but not surfaced.
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
    /// True when no skill was discovered — assembly then injects neither the
    /// intro nor the `read_skill` / `list_skill` tools.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// All entries in sorted-by-name order (deterministic regardless of
    /// filesystem enumeration order).
    pub fn entries(&self) -> &[SkillEntry] {
        &self.entries
    }

    /// Build an index from ready-made entries (sorted order is the caller's
    /// responsibility). Test-only helper — normal construction goes through
    /// [`scan`] (+ [`embedded_entry`] for built-ins).
    #[cfg(test)]
    pub(crate) fn from_entries(entries: Vec<SkillEntry>) -> Self {
        Self { entries }
    }

    /// Look up a full entry by name (linear scan — the index is small).
    pub fn entry_of(&self, name: &str) -> Option<&SkillEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Look up a skill's directory by name (linear scan — the index is small).
    /// `None` for embedded built-ins (they have no directory).
    pub fn dir_of(&self, name: &str) -> Option<&Path> {
        match self.entry_of(name).map(|e| &e.source) {
            Some(SkillSource::Dir(d)) => Some(d.as_path()),
            _ => None,
        }
    }

    /// Add a built-in skill UNLESS a disk skill of the same name already
    /// exists (disk wins — the user's override of a shipped guide). Insertion
    /// keeps the name-sorted order the scan established, so the intro list
    /// stays deterministic.
    pub fn insert_builtin(&mut self, e: SkillEntry) {
        if self.entry_of(&e.name).is_some() {
            return;
        }
        let pos = self
            .entries
            .partition_point(|x| x.name.as_str() < e.name.as_str());
        self.entries.insert(pos, e);
    }
}

/// Global skills dir: mirrors `main::default_config_path_in` (hand-rolled XDG,
/// no `dirs` crate) but joins `tinyagent/skills` instead of `tinyagent/config.json`.
/// Home resolution shares [`crate::config::pick_home`] with the config side —
/// the round-3 review R2-02 fix: a Windows-native shell without `$HOME` used to
/// make the global skills dir silently vanish while the config side (round-2
/// R2-P3-2) already fell back to `%USERPROFILE%`.
pub fn default_global_skills_dir() -> Option<PathBuf> {
    default_global_skills_dir_in(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
        std::env::var("USERPROFILE").ok().as_deref(),
    )
}

fn default_global_skills_dir_in(
    xdg: Option<&str>,
    home: Option<&str>,
    user_profile: Option<&str>,
) -> Option<PathBuf> {
    let base = match xdg {
        Some(x) if !x.is_empty() && Path::new(x).is_absolute() => PathBuf::from(x),
        _ => crate::config::pick_home(home, user_profile)
            .map(|h| PathBuf::from(h).join(".config"))?,
    };
    Some(base.join("tinyagent").join("skills"))
}

/// Cap on the bytes `scan` reads from one SKILL.md. Only the FRONTMATTER is
/// needed at scan time (name + description + optional fields — a few KiB at
/// most); the body is read later by `read_skill`, which has its own
/// max_output_chars cap. A hostile/corrupt SKILL.md of arbitrary size used
/// to be slurped whole into memory at every agent startup.
const SCAN_READ_CAP: u64 = 64 * 1024;

/// Cap on discovered skills. The level-1 intro is permanent system prompt;
/// 256 entries × ≤1024-char descriptions would already be an unusable prompt
/// — beyond this it's a misconfiguration or a hostile directory, and further
/// entries are dropped with a warning instead of bloating every request for
/// the whole session.
const MAX_SKILLS: usize = 256;

/// Hard cap on the rendered intro (chars). Same motivation as MAX_SKILLS,
/// one level finer: even 256 legal entries can overrun the prompt, so the
/// rendered list itself is bounded and says how much was omitted.
const INTRO_MAX_CHARS: usize = 16_384;

/// Scan the given dirs (pass global first, project last) and collect valid
/// skills. Missing dirs are silently skipped — normal when no skills are
/// installed. Malformed skills (no `SKILL.md`, missing frontmatter fields,
/// illegal name) are skipped and reported via `warnings` rather than aborting
/// assembly: one bad skill must not break agent startup.
pub fn scan(dirs: &[PathBuf]) -> (SkillIndex, Vec<String>) {
    let mut index = SkillIndex::default();
    let mut warnings = Vec::new();
    for dir in dirs {
        // The ROOT itself must be a real directory, same threat model as the
        // per-entry guard below one level up: `read_dir` follows a symlinked
        // root transparently, so a linked root (dotfiles farm / an untrusted
        // repo's `.tinyagent/skills -> /outside/tree`) would load every entry
        // from outside the skills tree with ZERO warnings — the entry-level
        // dir-only guard never sees a link at all (round-4 R4B-F1). A root
        // that doesn't exist at all stays the normal no-skills case (handled
        // by read_dir's NotFound arm below); every other read failure — a
        // plain-FILE path, a permission denial — warns instead of silently
        // killing the tier while malformed entries warn loudly (round-4
        // R4B-F7: rebalance that diagnostic asymmetry).
        if let Ok(m) = std::fs::symlink_metadata(dir) {
            if m.file_type().is_symlink() {
                warnings.push(format!(
                    "skills dir {} is a symlink/junction, skipped (skills roots must be real directories)",
                    dir.display()
                ));
                continue;
            }
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                continue; // missing dir is normal — no skills installed there
            }
            Err(e) => {
                warnings.push(format!(
                    "skills dir {} is not readable ({}), skipped",
                    dir.display(),
                    e
                ));
                continue;
            }
        };
        for e in entries.flatten() {
            // A skill directory must be a REAL directory: a symlinked entry
            // would source SKILL.md (and, via read_skill, every sidecar file)
            // from OUTSIDE the skills tree — e.g. an untrusted repo's
            // `.tinyagent/skills/x -> /some/dir` — injecting attacker-chosen
            // content into the SYSTEM PROMPT, a strictly more privileged
            // channel than the history a repo file normally reaches.
            // readdir's `file_type()` reports the link itself (no follow),
            // unlike `path().is_dir()`.
            match e.file_type() {
                Ok(ft) if ft.is_dir() => {}
                Ok(ft) if ft.is_symlink() => {
                    // A symlink/junction entry is refused (see above) — but
                    // SILENTLY skipping it left dotfiles-manager users (symlink
                    // farms) with a fully dead skills set and zero diagnostics,
                    // out of line with the "broken skill warned" contract
                    // (round-3 review R2-06). Warn once per entry; loose plain
                    // files stay silent (nothing suggests a skill was intended).
                    // The `continue` is the SECURITY half of this arm: without
                    // it the entry falls through to the SKILL.md reader and
                    // the link is FOLLOWED — loading skill content from
                    // outside the skills tree, the exact injection the
                    // dir-only guard exists to stop (caught on Windows by
                    // scan_warns_on_junctioned_skill_dir in Wave-2: the
                    // warn-only arm both warned AND loaded).
                    warnings.push(format!(
                        "skill dir {} is a symlink/junction, skipped (skills must be real directories)",
                        e.path().display()
                    ));
                    continue;
                }
                _ => continue,
            }
            let sub = e.path();
            let skill_md = sub.join("SKILL.md");
            // The SKILL.md read is the one file read in the whole skills stack
            // that bypasses `confined_to`: a file-level symlink here is
            // FOLLOWED by `read_capped`, putting attacker-chosen frontmatter
            // (the part that lands in the SYSTEM PROMPT) into the resident
            // intro while `read_skill` refuses the same link's body — loading
            // an intro the skill can never actually serve (round-4 R4B-F2).
            // Same refusal + warn contract as the directory-level guard above.
            match std::fs::symlink_metadata(&skill_md) {
                Ok(m) if m.file_type().is_symlink() => {
                    warnings.push(format!(
                        "skill dir {} SKILL.md is a symlink, skipped (skill files must be real files)",
                        sub.display()
                    ));
                    continue;
                }
                Ok(_) => {}
                Err(_) => {
                    warnings.push(format!(
                        "skill dir {} has no readable SKILL.md, skipped",
                        sub.display()
                    ));
                    continue;
                }
            }
            let content = match read_capped(&skill_md) {
                Some(c) => c,
                None => {
                    warnings.push(format!(
                        "skill dir {} has no readable SKILL.md, skipped",
                        sub.display()
                    ));
                    continue;
                }
            };
            let fm = match parse_frontmatter(&content) {
                FrontmatterParse::Parsed(fm) => fm,
                FrontmatterParse::Missing => {
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
                    source: SkillSource::Dir(sub.clone()),
                    license: fm.license,
                    compatibility: fm.compatibility,
                    allowed_tools: fm.allowed_tools,
                    metadata: fm.metadata,
                },
            );
        }
    }
    // Deterministic order (independent of filesystem readdir order): sort by
    // name so the system-prompt intro list is stable and predictable — the
    // MAX_SKILLS truncation below then keeps a deterministic subset.
    index.entries.sort_by(|a, b| a.name.cmp(&b.name));
    if index.entries.len() > MAX_SKILLS {
        warnings.push(format!(
            "more than {MAX_SKILLS} skills discovered; keeping the first {MAX_SKILLS} by name"
        ));
        index.entries.truncate(MAX_SKILLS);
    }
    (index, warnings)
}

/// Build an embedded (built-in) skill entry from a full `SKILL.md` text — the
/// same frontmatter validation as the disk scan (name/description required,
/// name charset rules, description ≤1024 chars), but no directory: the text is
/// the whole skill and `read_skill` serves it directly. Returns `None` on
/// malformed content; the caller skips with a warning (a broken built-in is a
/// build bug, never a startup failure).
pub fn embedded_entry(content: &str) -> Option<SkillEntry> {
    let FrontmatterParse::Parsed(fm) = parse_frontmatter(content) else {
        return None;
    };
    if !valid_skill_name(&fm.name) {
        return None;
    }
    if fm.description.chars().count() > 1024 {
        return None;
    }
    Some(SkillEntry {
        name: fm.name,
        description: fm.description,
        source: SkillSource::Embedded(content.to_string()),
        license: fm.license,
        compatibility: fm.compatibility,
        allowed_tools: fm.allowed_tools,
        metadata: fm.metadata,
    })
}

/// Read at most [`SCAN_READ_CAP`] bytes, lossily decoded (the cap can split a
/// multi-byte char at the boundary; the frontmatter parser is line-based and
/// tolerant). Returns `None` when the file can't be opened at all — the same
/// "skip + warn" contract the old `read_to_string` error path had.
fn read_capped(path: &Path) -> Option<String> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(SCAN_READ_CAP).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
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
#[derive(Debug)]
struct ParsedFrontmatter {
    name: String,
    description: String,
    license: Option<String>,
    compatibility: Option<String>,
    allowed_tools: Vec<String>,
    metadata: HashMap<String, String>,
}

/// Outcome of parsing a SKILL.md frontmatter block.
#[derive(Debug)]
enum FrontmatterParse {
    Parsed(ParsedFrontmatter),
    /// No frontmatter block at all, or a required field missing/empty.
    Missing,
}

/// Parse the agentskills.io YAML frontmatter block delimited by `---` lines.
///
/// All six standard fields are recognized: `name`/`description` (required),
/// `license`/`compatibility` (optional single-line), `allowed-tools` (optional,
/// whitespace-split into a list), and `metadata` (optional indented `key:
/// value` block → map). The four scalar fields (`name` / `description` /
/// `license` / `compatibility`) and each `metadata` value may be written
/// either as a single-line scalar or as a YAML BLOCK scalar (`|` literal or
/// `>` folded, with optional chomping / indentation indicators) — a form
/// the Agent Skills spec allows and real skill files use for long
/// descriptions. Block bodies are collected by indentation: every line
/// indented deeper than the `key:` line belongs to the block, the first
/// non-blank such line sets the block's base indentation, deeper lines keep
/// their extra indentation, and the first line at or above the key's
/// indentation ends it (so `metadata:` right after a folded description is
/// a sibling key, not description text). The value is always trimmed, so
/// the chomping indicator (`-`/`+`) is accepted but never changes the
/// result. `metadata` is otherwise the one block form and is collected by a
/// small indent-driven state machine. NOT supported, deliberately (no YAML
/// dependency): multi-line PLAIN scalars, block sequences (`- item`) and
/// nested mappings — such keys keep their pre-existing behavior (`allowed-
/// tools` reads empty, unknown keys are ignored, a required field comes out
/// empty → `Missing`). The open spec permits unknown keys, so they are
/// ignored (only Claude Code's upload path rejects them). Returns
/// [`FrontmatterParse::Missing`] if there is no frontmatter or either
/// required field is missing/empty. Expects the conventional `key: value`
/// shape (no space before the colon).
fn parse_frontmatter(content: &str) -> FrontmatterParse {
    // Strip a UTF-8 BOM if present: Windows editors (VS Code default, PowerShell
    // `>` redirection) commonly emit one, and U+FEFF is NOT whitespace
    // (char::is_whitespace returns false for it), so trim() below wouldn't
    // remove it — the first line would read `\u{feff}---` and fail the `== "---"`
    // check, silently skipping the whole skill with a misleading "missing
    // frontmatter" warning.
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    // `lines()` already normalizes CRLF (and a lone trailing CR) away, so a
    // block scalar collected from a Windows-checked-out skill carries no
    // stray `\r`.
    let lines: Vec<&str> = content.lines().collect();
    let Some(first) = lines.first() else {
        return FrontmatterParse::Missing;
    };
    if first.trim() != "---" {
        return FrontmatterParse::Missing;
    }
    let mut name = None;
    let mut description = None;
    let mut license = None;
    let mut compatibility = None;
    let mut allowed_tools: Vec<String> = Vec::new();
    let mut metadata: HashMap<String, String> = HashMap::new();
    let mut in_metadata = false;
    // `i` is the NEXT line to look at; a block scalar advances it past the
    // lines it consumed (the line that ENDED the block is looked at again,
    // as a key line).
    let mut i = 1;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
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
                    let text = scalar_value(v, &lines, &mut i, indent_of(line));
                    metadata.insert(k.to_string(), text);
                }
                continue;
            }
            in_metadata = false;
        }
        if let Some(rest) = t.strip_prefix("name:") {
            name = Some(scalar_value(rest, &lines, &mut i, indent_of(line)));
        } else if let Some(rest) = t.strip_prefix("description:") {
            description = Some(scalar_value(rest, &lines, &mut i, indent_of(line)));
        } else if let Some(rest) = t.strip_prefix("license:") {
            license = Some(scalar_value(rest, &lines, &mut i, indent_of(line)));
        } else if let Some(rest) = t.strip_prefix("compatibility:") {
            compatibility = Some(scalar_value(rest, &lines, &mut i, indent_of(line)));
        } else if let Some(v) = strip_key(t, "allowed-tools") {
            allowed_tools = v.split_whitespace().map(String::from).collect();
        } else if strip_key(t, "metadata").is_some() {
            // `metadata:` introduces an indented block; any inline value is
            // ignored (the spec models metadata as a map, not a scalar).
            in_metadata = true;
        }
        // other keys ignored
    }
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return FrontmatterParse::Missing;
    };
    let Some(description) = description.filter(|d| !d.is_empty()) else {
        return FrontmatterParse::Missing;
    };
    FrontmatterParse::Parsed(ParsedFrontmatter {
        name,
        description,
        license,
        compatibility,
        allowed_tools,
        metadata,
    })
}

/// The value of a scalar field whose text after `key:` is `rest`: a block
/// scalar body when `rest` is a block header (`|` / `>`), else the
/// single-line scalar (quotes stripped). `lines`/`i` are the frontmatter
/// line cursor (see `parse_frontmatter`); only the block form moves it.
/// `key_indent` is the key line's indentation — a block body must be
/// indented deeper than it.
fn scalar_value(rest: &str, lines: &[&str], i: &mut usize, key_indent: usize) -> String {
    match parse_block_header(rest.trim()) {
        Some(header) => collect_block_scalar(lines, i, key_indent, header),
        None => unquote(rest.trim()),
    }
}

/// A parsed YAML block scalar header: `|` (literal) or `>` (folded), each
/// optionally followed by a chomping indicator (`-` / `+`) and an explicit
/// indentation indicator (`1`–`9`), in either order.
struct BlockHeader {
    /// `>`: line breaks between equally-indented lines fold to spaces.
    /// `|`: every line break is kept.
    folded: bool,
    /// Explicit indentation indicator: the body is indented this many columns
    /// deeper than the key (instead of auto-detected from the first line).
    explicit_indent: Option<usize>,
}

/// Is the text after `key:` a YAML block scalar HEADER (`|` or `>`, with
/// optional chomping `+`/`-` and indentation digits in any order) and nothing
/// else? Strict match on purpose: `>2x faster` (trailing text) stays a plain
/// string — real-world descriptions may legitimately start with `>` (e.g.
/// prompts) — and YAML itself would reject a header with trailing garbage
/// (or a repeated indicator). Quoted values (`"|-"`) are plain scalars by
/// definition and never reach here. The chomping indicator is validated and
/// then ignored: block values are trimmed, so strip/clip/keep coincide.
fn parse_block_header(v: &str) -> Option<BlockHeader> {
    let mut chars = v.chars();
    let folded = match chars.next()? {
        '|' => false,
        '>' => true,
        _ => return None,
    };
    let (mut chomp, mut indent) = (false, None);
    for c in chars {
        match c {
            '-' | '+' if !chomp => chomp = true,
            '1'..='9' if indent.is_none() => indent = Some(c as usize - '0' as usize),
            _ => return None,
        }
    }
    Some(BlockHeader {
        folded,
        explicit_indent: indent,
    })
}

/// Number of leading space/tab columns of `line`.
fn indent_of(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

/// Collect a block scalar body starting at `lines[*i]`, for a header whose
/// key line is indented `key_indent` columns. Returns the trimmed scalar
/// value and the index of the first line NOT consumed by the block (the line
/// that ended it is re-examined as a key line by the caller).
///
/// Membership is by indentation only: a non-blank line belongs to the block
/// while it is indented deeper than the key line AND at least as deep as the
/// block's base indentation (the first non-blank body line's indentation, or
/// the explicit indicator). Blank lines inside are content (a paragraph
/// break); blank lines after the last body line are skipped. More-indented
/// lines keep their extra indentation relative to the base.
///
/// - literal (`|`): line breaks and blank lines are kept as-is.
/// - folded (`>`): the break between two equally-indented body lines becomes
///   one space; breaks next to a blank line or to a more-indented line stay
///   newlines (YAML's "more-indented lines are not folded" rule).
fn collect_block_scalar(
    lines: &[&str],
    i: &mut usize,
    key_indent: usize,
    header: BlockHeader,
) -> String {
    // `None` = blank line; `Some((indent, text))` = body line, whitespace at
    // both ends already removed (trailing whitespace is invisible in the
    // system prompt and would only make two equal descriptions compare
    // unequal; the leading indentation is re-added relative to `base`).
    let mut items: Vec<Option<(usize, &str)>> = Vec::new();
    let mut base = header.explicit_indent.map(|n| key_indent.saturating_add(n));
    while *i < lines.len() {
        let line = lines[*i];
        if line.trim().is_empty() {
            items.push(None);
            *i += 1;
            continue;
        }
        let indent = indent_of(line);
        if indent <= key_indent {
            break;
        }
        match base {
            None => base = Some(indent),
            Some(b) if indent < b => break,
            Some(_) => {}
        }
        // Text with BOTH ends' whitespace removed; the indentation is carried
        // separately (relative to `base`) so a body line never drags its
        // leading spaces into the value.
        items.push(Some((indent, line.trim())));
        *i += 1;
    }
    let base = base.unwrap_or(0);
    let body = |indent: usize, text: &str| -> String {
        let mut s = String::with_capacity(text.len() + 4);
        for _ in base..indent {
            s.push(' ');
        }
        s.push_str(text);
        s
    };
    let mut out = String::new();
    if header.folded {
        // `prev`: None before the first body line and after a blank line;
        // Some(more_indented) otherwise. A blank line after at least one body
        // line is a paragraph break (kept as a newline, and it suppresses the
        // fold on the following line).
        let mut prev: Option<bool> = None;
        for item in items {
            let Some((indent, text)) = item else {
                if prev.take().is_some() || !out.is_empty() {
                    out.push('\n');
                }
                continue;
            };
            let more_indented = indent > base;
            match prev {
                None => {}
                Some(false) if !more_indented => out.push(' '),
                Some(_) => out.push('\n'),
            }
            out.push_str(&body(indent, text));
            prev = Some(more_indented);
        }
    } else {
        for item in &items {
            match item {
                None => out.push('\n'),
                Some((indent, text)) => {
                    out.push_str(&body(*indent, text));
                    out.push('\n');
                }
            }
        }
    }
    out.trim().to_string()
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
    // Bound the rendered list: the intro is permanent system prompt and counts
    // toward the compression estimate, so a bulk install (or a directory full
    // of 1024-char descriptions) must not silently eat the context window.
    // Stop before overflowing INTRO_MAX_CHARS and say how much was omitted.
    let entries = index.entries();
    let mut shown = 0usize;
    for e in entries {
        let line = format!("- {}: {}\n", e.name, e.description);
        if out.chars().count() + line.chars().count() > INTRO_MAX_CHARS {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    if shown < entries.len() {
        out.push_str(&format!(
            "- （另有 {} 个技能省略：简介总量达到 {} 字符上限）\n",
            entries.len() - shown,
            INTRO_MAX_CHARS
        ));
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
        match parse_frontmatter(content) {
            FrontmatterParse::Parsed(fm) => fm,
            other => panic!("frontmatter should parse, got {other:?}"),
        }
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
        assert!(matches!(
            parse_frontmatter("no frontmatter here"),
            FrontmatterParse::Missing
        ));
        assert!(matches!(
            parse_frontmatter("---\ndescription: no name\n---\n"),
            FrontmatterParse::Missing
        ));
        assert!(matches!(
            parse_frontmatter("---\nname: no desc\n---\n"),
            FrontmatterParse::Missing
        ));
    }

    #[test]
    fn frontmatter_block_scalar_header_match_is_strict() {
        // Trailing text after the indicator is NOT a header (real prompts may
        // start with '>'): still parses as a plain single-line scalar.
        let f = fm("---\nname: x\ndescription: >2x faster than before\n---\nbody");
        assert_eq!(f.description, ">2x faster than before");
        let f = fm("---\nname: x\ndescription: > see the guide\n---\nbody");
        assert_eq!(f.description, "> see the guide");
        // Quoted block-scalar-looking value is a plain scalar by definition.
        let f = fm("---\nname: x\ndescription: \"|-\"\n---\nbody");
        assert_eq!(f.description, "|-");
        // Indicators: chomping + indentation in either order are headers; a
        // repeated indicator, indentation digit 0, or any other trailer is
        // not (YAML would reject those too).
        for h in [
            "|", ">", "|-", ">-", "|+", ">+", "|2", ">9", "|2-", ">-2", "|-2", ">2+",
        ] {
            assert!(parse_block_header(h).is_some(), "{h:?} must be a header");
        }
        for h in [
            "|--", ">-+", "|22", ">0", "|-x", "| #c", "|2x", "", " -", "a|", "||",
        ] {
            assert!(
                parse_block_header(h).is_none(),
                "{h:?} must not be a header"
            );
        }
    }

    #[test]
    fn frontmatter_block_scalars_fold_and_keep_lines() {
        // Real skill files write long descriptions as YAML block scalars
        // (the Agent Skills spec allows it). The line parser used to skip
        // such skills outright. Table-driven over the forms that matter:
        // folded vs literal, indentation, blank lines, indicators, CRLF.
        let cases: Vec<(&str, String, &str)> = vec![
            ("folded joins lines with a space", ">".into(), "a b"),
            ("folded strip indicator", ">-".into(), "a b"),
            (
                "folded keep indicator (values are trimmed)",
                ">+".into(),
                "a b",
            ),
            ("literal keeps line breaks", "|".into(), "a\nb"),
            ("literal strip indicator", "|-".into(), "a"),
            ("literal keeps blank lines", "|".into(), "a\n\nb"),
            ("folded folds across ONE blank line", ">".into(), "a\nb"),
            ("folded keeps N-1 of N blank lines", ">".into(), "a\n\nb"),
            (
                "folded keeps breaks around a deeper line (extra indent kept)",
                ">".into(),
                "a\n  code\nb",
            ),
            (
                "literal keeps a deeper line's EXTRA indentation",
                "|".into(),
                "a\n  code\nb",
            ),
            ("a less-indented line ends the block", ">".into(), "a"),
        ];
        let bodies = [
            "  a\n  b\n",
            "  a\n  b\n",
            "  a\n  b\n",
            "  a\n  b\n",
            "  a\n",
            "  a\n\n  b\n",
            "  a\n\n  b\n",
            "  a\n\n\n  b\n",
            "  a\n    code\n  b\n",
            "  a\n    code\n  b\n",
            "    a\n  b: 1\n",
        ];
        for ((label, header, want), body) in cases.into_iter().zip(bodies) {
            let c = format!("---\nname: x\ndescription: {header}\n{body}---\nbody");
            let f = fm(&c);
            assert_eq!(f.description, want, "{label}: {c:?}");
        }
    }

    #[test]
    fn frontmatter_block_scalar_edge_shapes() {
        // Trailing whitespace on a body line is stripped (invisible in the
        // system prompt, and it would make equal descriptions differ).
        let f = fm("---\nname: x\ndescription: >\n  a   \n  b\t\n---\n");
        assert_eq!(f.description, "a b");
        // Deeper base indentation is auto-detected from the first body line.
        let f = fm("---\nname: x\ndescription: >\n    a\n    b\n---\n");
        assert_eq!(f.description, "a b");
        // A tab counts as one indentation column.
        let f = fm("---\nname: x\ndescription: |\n\ta\n---\n");
        assert_eq!(f.description, "a");
        // Leading blank lines are not content.
        let f = fm("---\nname: x\ndescription: >\n\n\n  a\n---\n");
        assert_eq!(f.description, "a");
        // Explicit indentation indicator (1 column deeper than the key).
        let f = fm("---\nname: x\ndescription: |2\n  a\n  b\n---\n");
        assert_eq!(f.description, "a\nb");
        // CRLF line endings: no stray carriage return in the value.
        let f = fm("---\r\nname: x\r\ndescription: >\r\n  a\r\n  b\r\n---\r\n");
        assert_eq!(f.description, "a b");
        let f = fm("---\r\nname: x\r\ndescription: |\r\n  a\r\n\r\n  b\r\n---\r\n");
        assert_eq!(f.description, "a\n\nb");
        // A multi-line name is still a name the standard rejects (the scan
        // layer reports it as invalid) — but a one-line block name is fine.
        let f = fm("---\nname: |-\n  x\ndescription: d\n---\n");
        assert_eq!(f.name, "x");
        let f = fm("---\nname: |\n  x\n  y\ndescription: d\n---\n");
        assert_eq!(f.name, "x\ny");
        assert!(!valid_skill_name(&f.name));
    }

    #[test]
    fn frontmatter_block_scalar_ends_at_sibling_keys_and_coexists_with_metadata() {
        // Membership is by indentation: the first line at or above the key's
        // indentation ends the block and is parsed as a key again.
        let f = fm("---\nname: x\ndescription: >\n  Use it\n  well\nlicense: MIT\n---\n");
        assert_eq!(f.description, "Use it well");
        assert_eq!(f.license.as_deref(), Some("MIT"));
        // `metadata:` right after a folded description is a sibling key, not
        // description text.
        let f =
            fm("---\nname: x\ndescription: >\n  Use it\n  well\nmetadata:\n  author: alice\n---\n");
        assert_eq!(f.description, "Use it well");
        assert_eq!(f.metadata.get("author").map(String::as_str), Some("alice"));
        // Optional scalar fields accept block scalars too.
        let f = fm("---\nname: x\ndescription: d\nlicense: >\n  Apache\n  2.0\n---\n");
        assert_eq!(f.license.as_deref(), Some("Apache 2.0"));
        // …and so does a metadata value; a sibling metadata key at the map's
        // own indentation ends it.
        let f = fm(
            "---\nname: x\ndescription: d\nmetadata:\n  notes: |\n    line1\n    line2\n  author: bob\n---\n",
        );
        assert_eq!(
            f.metadata.get("notes").map(String::as_str),
            Some("line1\nline2")
        );
        assert_eq!(f.metadata.get("author").map(String::as_str), Some("bob"));
        let f = fm(
            "---\nname: x\ndescription: d\nmetadata:\n  notes: >\n    line1\n    line2\n  author: bob\n---\n",
        );
        assert_eq!(
            f.metadata.get("notes").map(String::as_str),
            Some("line1 line2")
        );
    }

    #[test]
    fn frontmatter_empty_block_scalar_on_a_required_field_is_missing() {
        // A header with no body is an empty value: for a required field that
        // is the same loud `Missing` as an absent key (never an empty
        // description in the system prompt); for an optional field it is
        // Some(""), same as `license:` with nothing after the colon.
        assert!(matches!(
            parse_frontmatter("---\nname: x\ndescription: >\n---\n"),
            FrontmatterParse::Missing
        ));
        assert!(matches!(
            parse_frontmatter("---\nname: x\ndescription: |\nlicense: MIT\n---\n"),
            FrontmatterParse::Missing
        ));
        let f = fm("---\nname: x\ndescription: d\nlicense: |\n---\n");
        assert_eq!(f.license.as_deref(), Some(""));
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
        assert!(matches!(
            &shared.source,
            SkillSource::Dir(d) if d.starts_with(&project)
        ));
        // order is sorted by name (independent of readdir order)
        let names: Vec<&str> = idx.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["only-g", "shared"]);
        std::fs::remove_dir_all(&global).ok();
        std::fs::remove_dir_all(&project).ok();
    }

    /// The E0′ eval apparatus (code_llm M4): the registered variant file
    /// (`examples/eval/e0prime-skill/run-code/SKILL.md`, deployed by copying
    /// into a task workdir's `.tinyagent/skills/`) must actually load through
    /// the REAL scan — a shape drift between the apparatus file and the scan
    /// contract (frontmatter caps, name rules) would otherwise only surface
    /// mid-eval. The CI fingerprint gate (scripts/check_eval_apparatus.py)
    /// pins the file's content; this pins its loadability.
    #[test]
    fn scan_loads_the_e0prime_eval_apparatus() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/eval/e0prime-skill/run-code");
        assert!(src.join("SKILL.md").is_file(), "apparatus missing: {src:?}");
        let workdir = tmpd("e0p");
        let skills_dir = workdir.join(".tinyagent").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        // Copy the dir as the assembly doc instructs evaluators to.
        fs_extra_copy_dir(&src, &skills_dir.join("run-code")).unwrap();
        let (idx, warns) = scan(&[skills_dir]);
        assert!(
            warns.is_empty(),
            "apparatus scanned with warnings: {warns:?}"
        );
        let e = idx
            .entries()
            .iter()
            .find(|e| e.name == "run-code")
            .expect("apparatus loads as run-code");
        assert!(
            matches!(&e.source, SkillSource::Dir(d) if d.ends_with("run-code")),
            "loaded from the copied dir, not a stale built-in: {:?}",
            e.source
        );
        // The frozen A③ boundary sentence is what the arm is supposed to
        // measure — its lead line must be in the served body.
        let body = std::fs::read_to_string(src.join("SKILL.md")).unwrap();
        assert!(
            body.contains("面对剩余任务先三问"),
            "boundary sentence missing from the apparatus"
        );
        std::fs::remove_dir_all(&workdir).ok();
    }

    /// `fs::copy` per file — no fs_extra dep for one test.
    fn fs_extra_copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let to = dst.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                fs_extra_copy_dir(&entry.path(), &to)?;
            } else {
                std::fs::copy(entry.path(), &to)?;
            }
        }
        Ok(())
    }

    #[test]
    fn scan_silently_skips_missing_dir() {
        let (idx, warns) = scan(&[PathBuf::from("/nonexistent/ta-skill-dir-xyz")]);
        assert!(idx.is_empty());
        assert!(warns.is_empty()); // missing dir is normal, not a warning
    }

    #[test]
    fn scan_reads_only_the_head_of_skill_md() {
        // P2-1: scan needs only the frontmatter, so the read is capped at
        // SCAN_READ_CAP. Frontmatter placed PAST the cap must not be found
        // (that's the proof the read stopped); frontmatter at the top of an
        // equally huge file loads fine (the cap doesn't break normal skills).
        let dir = tmpd("capread");
        let sub = dir.join("far");
        std::fs::create_dir_all(&sub).unwrap();
        let pad = "x".repeat(SCAN_READ_CAP as usize + 1024);
        std::fs::write(
            sub.join("SKILL.md"),
            format!("{pad}\n---\nname: far\ndescription: f\n---\n"),
        )
        .unwrap();
        let (idx, warns) = scan(std::slice::from_ref(&dir));
        assert!(idx.is_empty(), "frontmatter past the cap must not be found");
        assert!(warns.iter().any(|w| w.contains("frontmatter")), "{warns:?}");

        let dir2 = tmpd("capread2");
        let sub2 = dir2.join("near");
        std::fs::create_dir_all(&sub2).unwrap();
        std::fs::write(
            sub2.join("SKILL.md"),
            format!("---\nname: near\ndescription: n\n---\n{pad}"),
        )
        .unwrap();
        let (idx2, warns2) = scan(std::slice::from_ref(&dir2));
        assert!(warns2.is_empty(), "{warns2:?}");
        assert!(idx2.dir_of("near").is_some());
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&dir2).ok();
    }

    #[test]
    fn scan_caps_total_skill_count() {
        // P2-2: the level-1 intro is permanent system prompt; an unbounded
        // directory of skills used to translate 1:1 into unbounded prompt
        // growth. Keep the first MAX_SKILLS by name (deterministic post-sort)
        // and say so in a warning.
        let dir = tmpd("manyskills");
        for i in 0..MAX_SKILLS + 10 {
            write_skill(
                &dir,
                &format!("skill-{i:04}"),
                &format!("---\nname: skill-{i:04}\ndescription: d{i}\n---\n"),
            );
        }
        let (idx, warns) = scan(std::slice::from_ref(&dir));
        assert_eq!(idx.entries().len(), MAX_SKILLS);
        // First-by-name subset: the numerically largest names were dropped.
        assert!(idx.dir_of("skill-0000").is_some());
        assert!(idx
            .dir_of(&format!("skill-{:04}", MAX_SKILLS + 9))
            .is_none());
        assert!(
            warns.iter().any(|w| w.contains("keeping the first")),
            "{warns:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_symlinked_skill_dir() {
        // A symlinked skill directory would source SKILL.md (and via
        // read_skill every sidecar) from outside the skills tree — an
        // untrusted repo could plant `.tinyagent/skills/x -> /etc/...` and
        // inject chosen content into the SYSTEM PROMPT. readdir's file_type
        // reports the link without following, so the entry is skipped.
        let real = tmpd("symlink-real");
        write_skill(
            &real,
            "outside",
            "---\nname: outside\ndescription: o\n---\n",
        );
        let dir = tmpd("symlink-skills");
        std::os::unix::fs::symlink(real.join("outside"), dir.join("outside")).unwrap();
        let (idx, warns) = scan(std::slice::from_ref(&dir));
        assert!(idx.is_empty(), "symlinked skill dir must be skipped");
        // Refused AND named (round-3 review R2-06): a symlink farm (a
        // dotfiles manager's whole skills set) used to die silently, out of
        // line with the "broken skill warned" contract. Loose plain files
        // stay silent; only the link-shaped skip warns.
        assert!(
            warns.len() == 1 && warns[0].contains("symlink/junction, skipped"),
            "{warns:?}"
        );
        std::fs::remove_dir_all(&real).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_intro_caps_total_size() {
        // P2-2: even legal entries (1024-char descriptions are in-spec) can
        // overrun the prompt in bulk; the rendered list must stop at
        // INTRO_MAX_CHARS and state how many entries were omitted.
        let mut entries = Vec::new();
        for i in 0..64 {
            entries.push(SkillEntry {
                name: format!("skill-{i:03}"),
                description: "x".repeat(1024),
                source: SkillSource::Dir(PathBuf::from("/x")),
                ..Default::default()
            });
        }
        let idx = SkillIndex { entries };
        let out = render_intro(None, &idx);
        assert!(
            out.chars().count() < INTRO_MAX_CHARS + 200,
            "{} chars",
            out.chars().count()
        );
        assert!(out.contains("省略"), "omission note expected: …");
        // A small index renders every entry with no note.
        let small = SkillIndex {
            entries: vec![SkillEntry {
                name: "a".into(),
                description: "d".into(),
                source: SkillSource::Dir(PathBuf::from("/x")),
                ..Default::default()
            }],
        };
        let out2 = render_intro(None, &small);
        assert!(out2.contains("- a: d"));
        assert!(!out2.contains("省略"));
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
                source: SkillSource::Dir(PathBuf::from("/x/deploy")),
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
                source: SkillSource::Dir(PathBuf::from("/x")),
                ..Default::default()
            }],
        };
        let out = render_intro(None, &idx);
        assert!(out.starts_with("## 可用技能"));
    }

    #[test]
    fn embedded_entry_parses_and_validates() {
        let e = embedded_entry("---\nname: run-code\ndescription: guide\n---\nbody")
            .expect("valid built-in content should parse");
        assert_eq!(e.name, "run-code");
        assert_eq!(e.description, "guide");
        assert!(matches!(e.source, SkillSource::Embedded(_)));
        // Same validation rules as the disk scan.
        assert!(embedded_entry("---\ndescription: no name\n---\n").is_none());
        assert!(embedded_entry("---\nname: bad name\ndescription: d\n---\n").is_none());
        let over = "x".repeat(1025);
        assert!(embedded_entry(&format!("---\nname: a\ndescription: {over}\n---\n")).is_none());
    }

    #[test]
    fn insert_builtin_respects_disk_override_and_order() {
        let disk = SkillEntry {
            name: "run-code".into(),
            description: "FROM DISK".into(),
            source: SkillSource::Dir(PathBuf::from("/x/run-code")),
            ..Default::default()
        };
        let mut idx = SkillIndex::from_entries(vec![disk]);
        idx.insert_builtin(
            embedded_entry("---\nname: run-code\ndescription: built-in\n---\nb").unwrap(),
        );
        // Disk wins: the built-in is a no-op.
        assert_eq!(idx.entry_of("run-code").unwrap().description, "FROM DISK");

        // Without a clash the built-in lands, in name-sorted position.
        let mut idx = SkillIndex::from_entries(vec![SkillEntry {
            name: "zeta".into(),
            description: "z".into(),
            ..Default::default()
        }]);
        idx.insert_builtin(
            embedded_entry("---\nname: alpha\ndescription: built-in\n---\nb").unwrap(),
        );
        let names: Vec<&str> = idx.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
        assert!(matches!(
            idx.entry_of("alpha").unwrap().source,
            SkillSource::Embedded(_)
        ));
        // An embedded skill has no directory.
        assert!(idx.dir_of("alpha").is_none());
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
            default_global_skills_dir_in(Some(xdg), None, None),
            Some(PathBuf::from(xdg).join("tinyagent").join("skills"))
        );
        assert_eq!(
            default_global_skills_dir_in(None, Some("/home/me"), None),
            Some(PathBuf::from("/home/me/.config/tinyagent/skills"))
        );
        // empty xdg falls back to home
        assert_eq!(
            default_global_skills_dir_in(Some(""), Some("/home/me"), None),
            Some(PathBuf::from("/home/me/.config/tinyagent/skills"))
        );
        // non-absolute xdg is ignored
        assert_eq!(
            default_global_skills_dir_in(Some("rel/path"), Some("/home/me"), None),
            Some(PathBuf::from("/home/me/.config/tinyagent/skills"))
        );
        assert_eq!(default_global_skills_dir_in(None, None, None), None);
        // R2-02: no HOME → USERPROFILE fallback (same precedence as the
        // config side's pick_home; Windows-native shells don't set HOME).
        // Expected paths are BUILT with join(), never spelled as literals:
        // PathBuf PartialEq compares components, and on unix a
        // backslash-spelled literal is ONE component (`C:\Users\me\...`
        // contains no `/`) while the join()-built actual carries `/` —
        // the mismatch broke this pin on every linux CI runner while
        // windows passed (components equality folds both separators).
        // The test's own header comment has warned about exactly this
        // since before round-3; the round-3 additions violated it.
        assert_eq!(
            default_global_skills_dir_in(None, None, Some("C:\\Users\\me")),
            Some(
                PathBuf::from("C:\\Users\\me")
                    .join(".config")
                    .join("tinyagent")
                    .join("skills")
            )
        );
        // Non-empty HOME still wins over USERPROFILE (pick_home precedence).
        assert_eq!(
            default_global_skills_dir_in(None, Some("/h"), Some("C:\\Users\\me")),
            Some(
                PathBuf::from("/h")
                    .join(".config")
                    .join("tinyagent")
                    .join("skills")
            )
        );
        // Empty HOME falls through too.
        assert_eq!(
            default_global_skills_dir_in(None, Some(""), Some("C:\\Users\\me")),
            Some(
                PathBuf::from("C:\\Users\\me")
                    .join(".config")
                    .join("tinyagent")
                    .join("skills")
            )
        );
    }

    /// W2-08（round-3 Wave-2）: symlink/junction 跳过警告的 Windows 半边——
    /// unix 钉（scan_skips_symlinked_skill_dir）只证明共享臂的 unix 门，
    /// Windows CI 对 junction→warn 无回归保护。junction 用 `mklink /J`
    /// （无需管理员特权，与 alias 探测钉同款手法）。
    #[cfg(windows)]
    #[test]
    fn scan_warns_on_junctioned_skill_dir() {
        let real = tmpd("junc-real");
        write_skill(
            &real,
            "outside",
            "---
name: outside
description: o
---
",
        );
        let dir = tmpd("junc-skills");
        let st = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(dir.join("outside"))
            .arg(real.join("outside"))
            .status()
            .expect("spawn cmd for mklink /J");
        assert!(st.success(), "junction creation failed");
        let (idx, warns) = scan(std::slice::from_ref(&dir));
        assert!(idx.is_empty(), "junctioned skill dir must be skipped");
        assert!(
            warns.len() == 1 && warns[0].contains("symlink/junction, skipped"),
            "{warns:?}"
        );
        std::fs::remove_dir_all(&real).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// R4B-F1: the skills ROOT being a symlink/junction bypasses the entry-
    /// level dir-only guard entirely (read_dir follows it transparently and
    /// every entry loads with zero warnings). The root itself must be refused
    /// + warned. Unix half (real symlinks; CI test job runs ubuntu/macos).
    #[cfg(unix)]
    #[test]
    fn scan_warns_on_symlinked_skills_root() {
        use std::os::unix::fs::symlink;
        let real = tmpd("rootlink-real");
        write_skill(
            &real,
            "outside",
            "---\nname: outside\ndescription: o\n---\n",
        );
        let link = tmpd("rootlink-link");
        let link_path = link.join("skills");
        symlink(&real, &link_path).expect("create root symlink");
        let (idx, warns) = scan(std::slice::from_ref(&link_path));
        assert!(idx.is_empty(), "symlinked skills root must be skipped");
        assert!(
            warns.len() == 1 && warns[0].contains("symlink/junction, skipped"),
            "{warns:?}"
        );
        std::fs::remove_dir_all(&real).ok();
        std::fs::remove_file(&link_path).ok();
        std::fs::remove_dir_all(&link).ok();
    }

    /// R4B-F1 Windows half: junctioned root (mklink /J, no admin privileges —
    /// same technique as the entry-level junction pin above).
    #[cfg(windows)]
    #[test]
    fn scan_warns_on_junctioned_skills_root() {
        let real = tmpd("rootjunc-real");
        write_skill(
            &real,
            "outside",
            "---\nname: outside\ndescription: o\n---\n",
        );
        let dir = tmpd("rootjunc-skills");
        let link = dir.join("skills");
        let st = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&real)
            .status()
            .expect("spawn cmd for mklink /J");
        assert!(st.success(), "junction creation failed");
        let (idx, warns) = scan(std::slice::from_ref(&link));
        assert!(idx.is_empty(), "junctioned skills root must be skipped");
        assert!(
            warns.len() == 1 && warns[0].contains("symlink/junction, skipped"),
            "{warns:?}"
        );
        std::fs::remove_dir_all(&real).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// R4B-F2: a file-level symlinked SKILL.md is the one read in the skills
    /// stack that bypasses confined_to — the link must be refused + warned so
    /// attacker frontmatter can't ride into the resident intro while the body
    /// stays unreadable. Unix only (Windows file symlinks need privileges).
    #[cfg(unix)]
    #[test]
    fn scan_skips_symlinked_skill_md_file() {
        use std::os::unix::fs::symlink;
        let dir = tmpd("mdlink-skills");
        let sub = dir.join("linked");
        std::fs::create_dir_all(&sub).unwrap();
        let outside = tmpd("mdlink-outside");
        let outside_md = outside.join("SKILL.md");
        std::fs::write(
            &outside_md,
            "---\nname: linked\ndescription: injected\n---\n",
        )
        .unwrap();
        symlink(&outside_md, sub.join("SKILL.md")).expect("create SKILL.md symlink");
        let (idx, warns) = scan(std::slice::from_ref(&dir));
        assert!(idx.is_empty(), "symlinked SKILL.md must be skipped");
        assert!(
            warns.len() == 1 && warns[0].contains("SKILL.md is a symlink"),
            "{warns:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    /// R4B-F7: a skills path that is a plain FILE (true misconfiguration) used
    /// to be silently swallowed as "missing", killing the whole tier with zero
    /// diagnostics while malformed entries warned loudly. It must warn.
    #[test]
    fn scan_warns_when_skills_path_is_a_file() {
        let dir = tmpd("notdir");
        let file = dir.join("skills");
        std::fs::write(&file, "not a directory").unwrap();
        let (idx, warns) = scan(std::slice::from_ref(&file));
        assert!(idx.is_empty());
        assert!(
            warns.len() == 1 && warns[0].contains("is not readable"),
            "{warns:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A `|`/`>` block scalar on description is legal YAML the Agent Skills
    /// spec allows; such a skill LOADS (it used to be skipped with a warning
    /// because the line parser couldn't serve the form), with the folded
    /// value as the one-line intro — and the header text never leaks into
    /// the system prompt.
    #[test]
    fn scan_loads_block_scalar_descriptions() {
        let dir = tmpd("blockscalar");
        write_skill(
            &dir,
            "folded",
            "---\nname: folded\ndescription: >-\n  a folded\n  multi-line intro\n---\nbody\n",
        );
        write_skill(
            &dir,
            "literal",
            "---\nname: literal\ndescription: |\n  first line\n\n  second paragraph\n---\nbody\n",
        );
        let (idx, warns) = scan(std::slice::from_ref(&dir));
        assert!(
            warns.is_empty(),
            "block-scalar skills are no longer a defect: {warns:?}"
        );
        let by_name: HashMap<&str, &SkillEntry> =
            idx.entries.iter().map(|e| (e.name.as_str(), e)).collect();
        assert_eq!(
            by_name.get("folded").map(|e| e.description.as_str()),
            Some("a folded multi-line intro")
        );
        assert_eq!(
            by_name.get("literal").map(|e| e.description.as_str()),
            Some("first line\n\nsecond paragraph")
        );
        assert_eq!(
            render_intro(None, &idx).matches("description: >").count(),
            0
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
