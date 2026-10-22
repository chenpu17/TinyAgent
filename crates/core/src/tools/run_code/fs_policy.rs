//! deny_paths / fs_write_dirs interception (design §7): path normalization,
//! symlink-traversal rules, and the segment-wise glob matcher.

use super::*;

// ── deny_paths / fs_write_dirs interception (design §7) ─────────────────────
//
// Positioning (D13): this guards the model MISREADING a path ("back up my env
// file" landing on `.env`), not a hostile guest — shell passes `cat .env`
// straight through when it's whitelisted, so this is not a security boundary.
// The hostile-guest remedies are restrict_to_workdir, hooks Deny, or dropping
// shell from the whitelist (A-8 pins the pass-through).

/// The file tools whose `path` argument the layer judges. Everything else
/// (shell, MCP, task, …) is out of scope by design. `copy_bytes` is judged
/// separately (two paths, [`intercept_copy`]) but lives in the same layer.
pub(super) const FILE_TOOLS: [&str; 4] = ["read_file", "write_file", "edit_file", "list_dir"];
pub(super) const WRITE_TOOLS: [&str; 2] = ["write_file", "edit_file"];

/// Judge one inner call. `Some(err)` = rejected (the error text becomes the
/// guest-visible exception); `None` = pass through to the tool.
pub(super) fn intercept_file_paths(
    name: &str,
    effective: &Value,
    snap: &CodeConfig,
    workdir_canon: &Path,
) -> Option<String> {
    if name == "copy_bytes" {
        return intercept_copy(effective, snap, workdir_canon);
    }
    if !FILE_TOOLS.contains(&name) {
        return None; // A-8: shell & friends are deliberately not intercepted
    }
    let Some(path) = effective.get("path").and_then(Value::as_str) else {
        return None; // the tool itself reports a missing/ill-typed path
    };
    let is_write = WRITE_TOOLS.contains(&name);

    // 中-1 (whole-chain, per sec-review M-1): writes reject a symlink in ANY
    // component under the workdir, not just the final one. A symlinked
    // component makes the write's real destination unknowable from the
    // spelling the model chose — `out -> /tmp/pwn` means `out/x` lands
    // outside the workdir even though the lexical form says otherwise; a
    // dangling `ln -s .env bak` lands the write on `.env` while the model
    // chose "bak". With NO symlink component under the canonical workdir the
    // lexical form IS the resolved form, so the policy judgment below is
    // sound. (TOCTOU between check and write stays the documented best-effort
    // boundary, same as confined_to.)
    if is_write && write_traverses_symlink(workdir_canon, path) {
        return Some(format!(
            "run_code: refusing to write through a symlink ('{path}') — \
             write the real path instead"
        ));
    }

    // Normalize to a workdir-relative path (中-2): independent of
    // `confined_to` (whose semantics bind to restrict_to_workdir), so this
    // layer holds with either sandbox mode. `./x`, `a/../x`, absolute-inside
    // spellings all collapse to one comparable form.
    let rel = match workdir_relative(workdir_canon, path) {
        Some(r) => r,
        None => {
            if is_write {
                // Guest writes stay inside the workdir, period — the
                // fs_write_dirs model is a permission LIST bounded by the
                // workdir ("empty = the whole workdir is writable"), not a
                // mirror of restrict_to_workdir's off mode.
                return Some(format!(
                    "run_code: guest writes are limited to the working \
                     directory ('{path}' is outside)"
                ));
            }
            // Reads outside workdir: deny_paths cannot see them — the
            // documented boundary (中-2). The remedy for hostile guests is
            // restrict_to_workdir or a hooks Deny.
            return None;
        }
    };

    if deny_match(&snap.deny_paths, &rel) {
        return Some(format!(
            "run_code: path '{path}' matches a code.deny_paths pattern"
        ));
    }
    if is_write && !snap.fs_write_dirs.is_empty() && !under_write_dir(&snap.fs_write_dirs, &rel) {
        return Some(format!(
            "run_code: write target '{path}' is outside code.fs_write_dirs"
        ));
    }
    None
}

/// The `copy_bytes` arm of [`intercept_file_paths`] (v2 review): a copy is a
/// read of `src` AND a write of `dst`, so each side gets its own policy —
/// `src` under READ rules (deny_paths only; outside-workdir reads stay the
/// documented 中-2 boundary), `dst` under WRITE rules (symlink-traversal
/// refusal, workdir confinement, deny_paths, fs_write_dirs — same order as
/// the main arm). Missing/ill-typed fields pass through as `None` so the
/// tool itself reports them, mirroring the main arm.
pub(super) fn intercept_copy(
    effective: &Value,
    snap: &CodeConfig,
    workdir_canon: &Path,
) -> Option<String> {
    let src = effective.get("src").and_then(Value::as_str)?;
    let dst = effective.get("dst").and_then(Value::as_str)?;
    // dst first — a refused write should fail before the read is even judged.
    if write_traverses_symlink(workdir_canon, dst) {
        return Some(format!(
            "run_code: refusing to write through a symlink ('{dst}') — \
             write the real path instead"
        ));
    }
    let rel = match workdir_relative(workdir_canon, dst) {
        Some(r) => r,
        None => {
            return Some(format!(
                "run_code: guest writes are limited to the working \
                 directory ('{dst}' is outside)"
            ))
        }
    };
    if deny_match(&snap.deny_paths, &rel) {
        return Some(format!(
            "run_code: path '{dst}' matches a code.deny_paths pattern"
        ));
    }
    if !snap.fs_write_dirs.is_empty() && !under_write_dir(&snap.fs_write_dirs, &rel) {
        return Some(format!(
            "run_code: write target '{dst}' is outside code.fs_write_dirs"
        ));
    }
    if let Some(rel) = workdir_relative(workdir_canon, src) {
        if deny_match(&snap.deny_paths, &rel) {
            return Some(format!(
                "run_code: path '{src}' matches a code.deny_paths pattern"
            ));
        }
    }
    None
}

/// The guest-only `copy_bytes` tool (v2 review): byte-exact file copy that
/// never round-trips content through a JS string — the model decides src and
/// dst, the host streams the bytes (binary, invalid-UTF-8, and oversized-
/// for-guest-memory files all survive; the raw+content_b64 recipe in the
/// skill is the manual version). Registered ONLY in the sandbox's guest
/// registry — never the agent's top-level list — so it is a run_code
/// primitive, not a capability the plain (non-code) route gains. Policy is
/// judged in [`intercept_copy`] before this runs; this body only re-resolves
/// paths (with the tool-side restrict_to_workdir sandbox, mirroring
/// read_file/write_file) and streams.
pub(super) struct CopyBytesTool;

impl Tool for CopyBytesTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "copy_bytes".into(),
            description: "字节保真复制文件（host 流式搬运，内容不经过 JS 字符串）：\
                          二进制/非 UTF-8/大文件均逐字节一致。适合导出、备份、迁移\
                          等需要保持原文的复制；要改内容不要用它。"
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "src": {"type": "string", "description": "源文件路径（相对 workdir）"},
                    "dst": {"type": "string", "description": "目标路径（须在 workdir 内）"}
                },
                "required": ["src", "dst"]
            }),
        }
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let src = input
            .get("src")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("copy_bytes: missing string field 'src'"))?;
        let dst = input
            .get("dst")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("copy_bytes: missing string field 'dst'"))?;
        let resolve = |p: &str, label: &str| {
            let full = absolutize(&ctx.workdir, p);
            if ctx.restrict_to_workdir {
                crate::tools::confined_to(&ctx.workdir, &full, label)
            } else {
                Ok(full)
            }
        };
        let (s, d) = match (resolve(src, "copy source"), resolve(dst, "copy target")) {
            (Ok(s), Ok(d)) => (s, d),
            (Err(e), _) | (_, Err(e)) => {
                return Ok(ToolOutput::error(format!("copy_bytes: {e:#}")))
            }
        };
        match std::fs::copy(&s, &d) {
            Ok(n) => Ok(ToolOutput::text(
                json!({"copied_bytes": n, "src": src, "dst": dst}).to_string(),
            )),
            Err(e) => Ok(ToolOutput::error(format!(
                "copy_bytes: {} -> {}: {e}",
                s.display(),
                d.display()
            ))),
        }
    }
}

/// Join a guest path onto the workdir unless absolute (the same resolution
/// the file tools themselves apply — the layer must judge the path the tool
/// would actually touch).
pub(super) fn absolutize(workdir: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        workdir.join(p)
    }
}

/// Normalize to the workdir-relative, `/`-separated form. Returns `None`
/// when the path resolves outside the workdir (deny_paths "not applicable")
/// or is too long/deep to be a real path (see [`MAX_REL_PATH_BYTES`] —
/// reads then fall through to the tool and fail naturally, writes are
/// denied). Canonicalize first (resolving symlinks that EXIST); on failure
/// (target not there yet — the normal write case) resolve the deepest
/// EXISTING ancestor of the ORIGINAL spelling and append the remainder
/// ([`resolve_via_existing_ancestor`]) — kernel-truth, unlike the previous
/// pure-lexical fallback, which collapsed `link/..` away on paper and judged
/// `/outside/../<workdir>/link/../x` as INSIDE while the kernel wrote it
/// outside (external re-review P1).
pub(super) fn workdir_relative(workdir_canon: &Path, path: &str) -> Option<String> {
    let full = absolutize(workdir_canon, path);
    let canon = match full.canonicalize() {
        Ok(c) => c,
        Err(_) => {
            // Payload guard BEFORE the ancestor walk: over-long spellings end
            // up denied (writes) / pass-through-then-ENOENT (reads) either
            // way — bound the one-canonicalize-per-level walk first
            // (sec-review M-2 shape).
            if path.len() > MAX_REL_PATH_BYTES {
                return None;
            }
            resolve_via_existing_ancestor(&full).unwrap_or_else(|| lexical_normalize(&full))
        }
    };
    let rel = canon.strip_prefix(workdir_canon).ok()?;
    let rel = rel.to_string_lossy().replace('\\', "/");
    if rel.len() > MAX_REL_PATH_BYTES || rel.split('/').count() > MAX_REL_SEGMENTS {
        return None;
    }
    Some(rel)
}

/// Kernel-truth resolution for a path whose tail does not exist yet (the
/// normal write case): canonicalize the deepest EXISTING ancestor of the
/// ORIGINAL spelling — resolving every symlink and `..` on the way, exactly
/// as the kernel will at write time — then append the not-yet-existing
/// remainder lexically. The remainder can hold no `..`: `file_name()` of a
/// `..`-terminated path is `None`, and that case bails to the caller's
/// lexical fallback — preserving the old behavior for `missing/../file`
/// spellings, which the tool layer answers (ENOENT or parent creation),
/// never a policy refusal.
///
/// `None` also past [`MAX_REL_SEGMENTS`] stripped components: deeper tails
/// are denied by `workdir_relative`'s segment check regardless, so the walk
/// stops before a guest-controlled depth turns it into a syscall-per-level
/// stall.
pub(super) fn resolve_via_existing_ancestor(full: &Path) -> Option<PathBuf> {
    let mut ancestor: &Path = full;
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(mut resolved) => {
                for comp in rest.iter().rev() {
                    resolved.push(comp);
                }
                return Some(resolved);
            }
            Err(_) => {
                if rest.len() >= MAX_REL_SEGMENTS {
                    return None;
                }
                rest.push(ancestor.file_name()?.to_os_string());
                ancestor = ancestor.parent()?;
            }
        }
    }
}

/// No real path resolves longer/deeper than this: PATH_MAX (4096) bounds
/// every syscall path argument, so past these bounds the string is not a
/// filename but a payload (sec-review M-2: a guest-controlled ~500k-segment
/// path used to drive [`glob_match`]'s recursive `**` arm into a host stack
/// overflow — an uncatchable abort no fuel/mem limiter sees).
pub(super) const MAX_REL_PATH_BYTES: usize = 4096;
pub(super) const MAX_REL_SEGMENTS: usize = 256;

/// Does the write target traverse a symlink component under the workdir?
/// Iterative — depth is bounded by the path guards and never guest-controlled
/// recursion. Two-stage judgment:
///
/// 1. Lexical screen: fold the spelling and bail early when it collapses
///    outside the workdir (the outside-workdir rule rejects those right
///    after — no walk needed).
/// 2. Symlink walk on the ORIGINAL spelling, component by component
///    (external re-review P1): the kernel applies `..` only AFTER resolving
///    the component in front of it — `link/../x` resolves THROUGH `link`,
///    and the tool writes the original spelling, so that is what the
///    kernel actually does at write time. The old code folded `..` FIRST,
///    which erases `link` from the walk and shows the policy layer a clean
///    path while the real write escapes. Walking original order —
///    push-and-check on Normal, pop on ParentDir — rejects a symlink in any
///    original position; every component that survives its check is a real
///    directory, so a lexical pop matches the kernel's resolution.
///
/// Absolute spellings walk ANCHORED AT THE FILESYSTEM ROOT (second external
/// P1): the old code `strip_prefix`'d the original absolute spelling and gave
/// up when its literal front differed from the canonical workdir — but
/// `/outside/../<workdir>/link/../x` collapses back inside (passing the
/// screen above) while never spelling the workdir as its literal prefix, so
/// the `link` it traverses went unchecked. Root-anchored walking covers every
/// spelling the screen admits: a component above the workdir in such a
/// spelling is either a real directory (lexical pop == kernel pop) or itself
/// a symlink — a resolution we cannot reason about lexically, which is
/// exactly what this layer refuses.
pub(super) fn write_traverses_symlink(workdir_canon: &Path, path: &str) -> bool {
    let full = lexical_normalize(&absolutize(workdir_canon, path));
    if full.strip_prefix(workdir_canon).is_err() {
        return false;
    }
    // Payload paths (sec-review M-2): the walk below pays one symlink_metadata
    // per segment — µs-scale on unix, ~100µs+ on Windows filter drivers (60k
    // segments measured 12s there). Over-limit spellings are denied
    // fail-closed by workdir_relative's length check right after regardless,
    // so skipping the walk only saves the doomed syscalls, never flips a
    // decision. Guard the ORIGINAL spelling: it is what drives the walk.
    if path.len() > MAX_REL_PATH_BYTES {
        return false;
    }
    let orig = Path::new(path);
    if orig.components().count() > MAX_REL_SEGMENTS {
        return false;
    }
    use std::path::Component;
    // Relative spellings anchor at the canonical workdir; absolute ones at
    // the filesystem root (see the fn doc — a literal strip_prefix of the
    // original spelling misses `x/../<workdir>/…` spellings entirely).
    let mut cur = if orig.is_absolute() {
        PathBuf::new()
    } else {
        workdir_canon.to_path_buf()
    };
    for comp in orig.components() {
        match comp {
            Component::Normal(c) => {
                cur.push(c);
                if std::fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink()) {
                    return true;
                }
            }
            Component::ParentDir => {
                cur.pop();
            }
            Component::CurDir => {}
            // Rebuild the absolute anchor: the Windows prefix ("C:") then the
            // root — pushing MAIN_SEPARATOR onto the empty/prefix-only buffer
            // yields "/" / "C:\". Neither the root nor a canonical workdir
            // prefix is itself a symlink, so not checking them loses nothing.
            Component::Prefix(p) => cur.push(p.as_os_str()),
            Component::RootDir => cur.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
        }
    }
    false
}

/// Lexical `.`/`..` collapse WITHOUT symlink resolution — used for the
/// escape-detection screen and as `workdir_relative`'s fallback-of-last-
///-resort (an ancestor chain whose not-yet-existing component precedes a
/// `..`, where the real write fails ENOENT regardless). `..` past the root
/// saturates (parts simply run empty — a no-op), matching the
/// escape-detection above (a path that lexical-collapses outside the
/// workdir fails `strip_prefix` → outside).
///
/// NOT `out.push(c.as_os_str())` per component: on Windows, per-component
/// `PathBuf::push` fed from a verbatim-joined long path costs ~1.5ms EACH
/// (60k segments measured 92s, while components() iteration and any
/// whole-string build are µs — Windows E2E finding, sec-review M-2 shape).
/// Collect, collapse, then assemble with amortized-O(1) OsString pushes:
/// linear on every OS, identical collapse semantics.
pub(super) fn lexical_normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut head = PathBuf::new(); // prefix + root, kept verbatim as-is
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => head.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(name) => parts.push(name.to_os_string()),
        }
    }
    let sep: &std::ffi::OsStr = std::path::MAIN_SEPARATOR_STR.as_ref();
    let mut out = head.into_os_string();
    for part in parts {
        // `PathBuf::push` inserts a separator only between existing content
        // and the part — an empty base (relative paths: head is empty) must
        // not grow a leading one, or "out" becomes "\out".
        if !out.is_empty() && !ends_with_sep(&out) {
            out.push(sep);
        }
        out.push(&part);
    }
    PathBuf::from(out)
}

/// The rightmost-byte separator check `PathBuf::push` performs, byte-level:
/// exact because UTF-8/WTF-8 never embeds 0x2F or 0x5C inside a multi-byte
/// sequence.
pub(super) fn ends_with_sep(s: &std::ffi::OsStr) -> bool {
    match s.as_encoded_bytes().last() {
        Some(b'/') => true,
        #[cfg(windows)]
        Some(b'\\') => true,
        _ => false,
    }
}

/// Does any deny pattern match the workdir-relative path?
pub(super) fn deny_match(patterns: &[String], rel: &str) -> bool {
    let folded: Vec<String> = fold_case(rel).split('/').map(String::from).collect();
    let path: Vec<&str> = folded.iter().map(String::as_str).collect();
    patterns.iter().any(|p| {
        // Pattern-side cleaning, identical in kind to `under_write_dir`'s
        // entries (audit follow-up — the two policy faces were asymmetric):
        // the rel side arrives already normalized to a clean `/`-separated
        // form, so a raw `./x`, `x/`, or `a//x` spelling, and on Windows the
        // `lib\x.js` habit, silently became a dead rule — fail-open against
        // the admin's intent. On unix a backslash is a legal filename byte,
        // so only Windows folds it as a separator.
        let cleaned = lexical_normalize(Path::new(p))
            .to_string_lossy()
            .into_owned();
        #[cfg(windows)]
        let cleaned = cleaned.replace('\\', "/");
        // A pattern that cleans to nothing matches nothing (same rule as
        // write-dir entries).
        !cleaned.is_empty() && {
            let folded_pat = fold_case(&cleaned);
            let pat: Vec<&str> = folded_pat.split('/').collect();
            glob_match(&pat, &path)
        }
    })
}

/// Containment against fs_write_dirs: `rel == dir` or `rel` nested under it.
/// Entries are lexical dir paths (not globs), cleaned with the same lexical
/// pass as guest paths — `./out`, `out/`, `out//x` all behave like their
/// clean spellings (an entry that cleans to nothing matches nothing).
pub(super) fn under_write_dir(dirs: &[String], rel: &str) -> bool {
    let rel = fold_case(rel);
    dirs.iter().any(|d| {
        let cleaned = lexical_normalize(Path::new(d));
        let d = fold_case(&cleaned.to_string_lossy());
        !d.is_empty() && (rel == d || rel.starts_with(&format!("{d}/")))
    })
}

/// Pattern-side magnitude bound for [`glob_match`] (sec-review P2): the PATH
/// side is capped at [`MAX_REL_SEGMENTS`] by the callers, but `deny_paths` is
/// admin config and unbounded — this is the post-collapse defensive cap past
/// which a pattern is declared non-matching (warn, never panic) instead of
/// being fed to the recursive matcher. 1024 is far past any sane deny rule
/// (the shipped defaults are ≤3 segments) while staying trivial for the
/// linear scan below.
pub(super) const MAX_GLOB_PATTERN_SEGMENTS: usize = 1024;

/// Segment-wise glob match: `*` stays inside a segment, a whole `**` segment
/// crosses any number of segments (zero included — `**/.env*` also hits the
/// workdir root), and a pattern with no `/` matches only the FIRST segment
/// (`.env*` does not hit `a/.env`). Classic recursive matcher over segments.
///
/// Entry hardening (sec-review P2): the `**` arm is a two-way recursion with
/// no memoization, so a stacked `**/**/**/…/x` pattern against an n-segment
/// path revisits the same (pattern-tail, path-tail) states C(n, stars) times
/// — a host-thread hang no fuel/mem limiter sees (standalone probe of the
/// bare matcher: 4 stars × 64 segs 0.85s, 5 stars 11.5s, 6 stars 131s,
/// ~12× per added star; 8 stars extrapolates to hours). The entry therefore
/// lexically collapses every run of consecutive `**` segments to ONE before
/// recursing. Equivalence: `**/** ≡ **` — a `**` segment matches zero-or-
/// more WHOLE segments, i.e. the full Kleene closure Σ* over segment
/// sequences, and Σ*·Σ* = Σ* (every total t ≥ 0 decomposes as t+0, and every
/// composition sums to a total the single star also covers), so both
/// spellings accept exactly the same (prefix-consumed, tail-match) splits
/// and the recursive predicate is bit-for-bit unchanged. The collapse kills
/// the stacked shape outright; the segment cap above backstops whatever a
/// non-stackable config typo produces (fail-closed, warn only).
pub(super) fn glob_match(pat: &[&str], path: &[&str]) -> bool {
    let mut flat: Vec<&str> = Vec::with_capacity(pat.len());
    for seg in pat {
        // `**/** ≡ **` (Kleene argument on the entry doc) — drop the extras.
        if *seg == "**" && flat.last() == Some(&"**") {
            continue;
        }
        flat.push(seg);
    }
    if flat.len() > MAX_GLOB_PATTERN_SEGMENTS {
        crate::diag::diag_warn!(
            "run_code: deny_paths pattern collapses to {} segments (cap \
             {MAX_GLOB_PATTERN_SEGMENTS}) — treated as non-matching",
            flat.len()
        );
        return false;
    }
    glob_match_inner(&flat, path)
}

/// The recursive core — INVARIANT: `pat` carries no two ADJACENT `**`
/// segments (the entry pass collapsed every run to one), so the exponential
/// stacked-star call tree cannot form below this point.
pub(super) fn glob_match_inner(pat: &[&str], path: &[&str]) -> bool {
    match (pat.split_first(), path.split_first()) {
        (Some((&"**", rest_p)), _) => {
            glob_match_inner(rest_p, path)
                || path
                    .split_first()
                    .is_some_and(|(_, rest_s)| glob_match_inner(pat, rest_s))
        }
        (Some((p, rest_p)), Some((s, rest_s))) => {
            seg_match(p, s) && glob_match_inner(rest_p, rest_s)
        }
        (None, None) => true,
        _ => false,
    }
}

/// One-segment wildcard match (`*` = any run of chars, no `/` crossing —
/// there is no `/` left inside a segment to cross).
pub(super) fn seg_match(pat: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    // DP over (pattern idx, subject idx) — small strings, no backtracking blowup.
    let mut dp = vec![vec![false; s.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for i in 1..=p.len() {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=p.len() {
        for j in 1..=s.len() {
            dp[i][j] = if p[i - 1] == '*' {
                dp[i - 1][j] || dp[i][j - 1]
            } else {
                dp[i - 1][j - 1] && p[i - 1] == s[j - 1]
            };
        }
    }
    dp[p.len()][s.len()]
}

/// Case-fold for path comparison. macOS (APFS default) and Windows volumes
/// are case-insensitive — `.ENV` must hit `.env` there. Linux keeps exact
/// case (over-matching would deny files that aren't the named secret).
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(super) fn fold_case(s: &str) -> String {
    s.to_lowercase()
}
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn fold_case(s: &str) -> String {
    s.to_string()
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── M2: interception layer + audit + call limit (design §7/§8.4) ─────────

    /// glob semantics vectors (§13-6 + review 低-3): `*` stays in-segment,
    /// whole-`**` segments cross (and match ZERO segments), a no-`/` pattern
    /// matches only the root's first segment, nested `.aws` needs its own
    /// pattern.
    #[test]
    fn glob_semantics_vectors() {
        let m = |pat: &str, path: &str| deny_match(&[pat.to_string()], path);
        // No-`/` patterns: root FIRST segment only.
        assert!(m(".env*", ".env"));
        assert!(m(".env*", ".env.local"));
        assert!(!m(".env*", "a/.env"), "no-/ pattern must not match nested");
        assert!(m("*.pem", "server.pem"));
        assert!(!m("*.pem", "keys/server.pem"));
        // `**/` matches zero or more segments — root included.
        assert!(m("**/.env*", ".env"));
        assert!(m("**/.env*", "a/.env"));
        assert!(m("**/.env*", "a/b/.env.local"));
        assert!(m("**/id_rsa*", "id_rsa"));
        assert!(m("**/id_rsa*", ".ssh/id_rsa.pub"));
        // `.aws` at root vs nested — both default entries matter.
        assert!(m(".aws/**", ".aws/config"));
        assert!(!m(".aws/**", "x/.aws/config"), "nested needs ** prefix");
        assert!(m("**/.aws/**", "x/.aws/config"));
        assert!(m("**/.aws/**", ".aws"), "`**` tail matches zero segments");
        // The directory ROOT itself (bare path, no trailing /) — external
        // review P2 asked whether `.aws/**`-style rules leak it to `list_dir`;
        // here the `**` tail matches zero segments, so both bare roots are
        // covered by the same entries (no gitignore-style "contents only").
        assert!(m(".aws/**", ".aws"), "bare root dir");
        assert!(m("**/.aws/**", "x/.aws"), "bare nested dir root");
        // `.git` as dir-tree and as file (linked worktrees) — both entries.
        assert!(m("**/.git/**", ".git/hooks/pre-commit"));
        assert!(m("**/.git/**", "sub/.git/config"));
        assert!(m("**/.git", "sub/.git"));
        // Mid-path `**`.
        assert!(m("a/**/b", "a/b"));
        assert!(m("a/**/b", "a/x/y/b"));
        assert!(!m("a/**/b", "a/c"));
        // Case folding on case-insensitive volumes (macOS/Windows builds).
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        assert!(m(".env*", ".ENV"), "case-insensitive volumes must fold");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert!(!m(".env*", ".ENV"), "case-sensitive volumes stay exact");
    }

    /// Pattern-side lexical cleaning (audit follow-up): the rel side arrives
    /// normalized, so `./x`, `x/`, `a//x` spellings — and on Windows the
    /// `lib\x.js` separator habit — must behave like their clean forms
    /// instead of being dead rules; a pattern that cleans to nothing (`.`,
    /// `/`) matches nothing. On unix a backslash stays a filename byte.
    #[test]
    fn deny_match_cleans_pattern_spellings_like_the_entry_side() {
        let m = |pat: &str, path: &str| deny_match(&[pat.to_string()], path);
        assert!(
            m(".aws/", ".aws"),
            "trailing separator is spelling, not meaning"
        );
        assert!(m("./.aws/**", ".aws/config"), "leading ./ is cleaned");
        assert!(m(".aws//**", ".aws/config"), "double separator collapses");
        assert!(
            !m(".", ".aws"),
            "pattern cleaning to nothing matches nothing"
        );
        assert!(!m("/", ".aws"), "bare root pattern matches nothing");
        #[cfg(windows)]
        assert!(
            m(r"lib\x.js", "lib/x.js"),
            "Windows backslash separator habit is folded"
        );
        #[cfg(not(windows))]
        assert!(
            !m(r"lib\x.js", "lib/x.js"),
            "unix backslash is a filename byte, not a separator"
        );
    }

    /// Sec-review P2: consecutive `**` segments are collapsed at the matcher
    /// entry (`**/** ≡ **`, the Kleene argument on glob_match's doc), so a
    /// stacked spelling must answer EXACTLY like its single-star twin across
    /// the whole vector board — pinned as paired equivalence plus direct
    /// truth anchors, so neither the collapse nor the matcher can drift.
    #[test]
    fn glob_stacked_doublestar_collapses_to_single_star_semantics() {
        let m = |pat: &str, path: &str| deny_match(&[pat.to_string()], path);
        for (stacked, single) in [
            ("**/**/.env*", "**/.env*"),
            ("**/**/**/id_rsa*", "**/id_rsa*"),
            ("a/**/**/b", "a/**/b"),
            ("**/**/.aws/**", "**/.aws/**"),
            (".aws/**/**", ".aws/**"),
            ("**/**", "**"),
        ] {
            for path in [
                ".env",
                "a/.env",
                "a/b/c/.env.local",
                "id_rsa",
                ".ssh/id_rsa.pub",
                "a/b",
                "a/x/y/z/b",
                ".aws",
                "x/.aws/config",
                "x/.aws",
                ".aws/config",
            ] {
                assert_eq!(
                    m(stacked, path),
                    m(single, path),
                    "{stacked} must equal {single} on {path:?}"
                );
            }
        }
        // Direct truth pins (equivalence alone could pass on a shared bug).
        assert!(m("**/**/.env*", ".env"), "leading stars cross zero segs");
        assert!(m("**/**/.env*", "a/b/.env"));
        assert!(!m("**/**/.env*", "a/b/other"));
        assert!(m("a/**/**/b", "a/b"));
        assert!(m("a/**/**/b", "a/x/y/b"));
        assert!(!m("a/**/**/b", "a/x"));
        assert!(m("**/**/.aws/**", "x/.aws/config"));
        assert!(m("**/**/.aws/**", ".aws"), "stacked tails cross zero segs");
        assert!(
            m(".aws/**/**", ".aws"),
            "trailing stack matches the bare root"
        );
        assert!(m("**/**", "a/b/c"), "bare stacked stars match anything");
    }

    /// Sec-review P2 halting vector: 8 consecutive `**` segments against a
    /// 64-segment NON-matching path is C(64,8) ≈ 4.4e9 recursive calls in
    /// the un-collapsed matcher (standalone probe: 4/5/6 stars took
    /// 0.85s/11.5s/131s, ~12× per star — 8 stars extrapolates to hours).
    /// The entry collapse reduces it to a linear `**/nomatch` scan. Both
    /// answers are pinned for correctness AND the wall clock gets a 2s
    /// bound — three orders above the collapsed matcher's microseconds, so
    /// a loaded CI box cannot flake it while a regression still trips it.
    #[test]
    fn glob_pathological_stacked_doublestar_pattern_terminates_fast() {
        let miss = (0..64)
            .map(|i| format!("s{i}"))
            .collect::<Vec<_>>()
            .join("/");
        let hit = (0..63)
            .map(|i| format!("s{i}"))
            .chain(std::iter::once("nomatch".to_string()))
            .collect::<Vec<_>>()
            .join("/");
        let t = Instant::now();
        assert!(
            !deny_match(&["**/**/**/**/**/**/**/**/nomatch".to_string()], &miss),
            "no-match twin must answer false"
        );
        assert!(
            deny_match(&["**/**/**/**/**/**/**/**/nomatch".to_string()], &hit),
            "match twin must answer true"
        );
        let elapsed = t.elapsed();
        assert!(
            elapsed.as_millis() < 2_000,
            "collapsed matcher is linear — took {elapsed:?}"
        );
    }

    /// The pattern-side segment cap (post-collapse magnitude bound on admin
    /// config): at the cap a matching pattern still matches; one segment
    /// past it the pattern is non-matching — fail-closed, warn, no hang, no
    /// panic. deny_match itself does not cap the PATH (its callers do), so
    /// the boundary is directly spellable here.
    #[test]
    fn glob_pattern_segment_cap_boundary_is_inclusive_and_fail_closed() {
        let at_cap = vec!["a"; MAX_GLOB_PATTERN_SEGMENTS].join("/");
        let over_cap = vec!["a"; MAX_GLOB_PATTERN_SEGMENTS + 1].join("/");
        let deep = vec!["a"; MAX_GLOB_PATTERN_SEGMENTS].join("/");
        assert!(
            deny_match(&[at_cap], &deep),
            "a pattern at the cap still matches"
        );
        assert!(
            !deny_match(&[over_cap], &deep),
            "past the cap the pattern is declared non-matching"
        );
    }

    /// A-7: deny_paths hits across oblique spellings — the workdir-relative
    /// normalization kills `./` and `a/../` bypasses before matching.
    #[test]
    fn deny_paths_block_env_reads_across_spellings() {
        let dir = scratch();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"
                    const spellings = [".env", "./.env", "sub/../.env"];
                    const results = [];
                    for (const p of spellings) {
                        try { tools.call("read_file", {path: p}); results.push(p + ":ran"); }
                        catch (e) { results.push(p + ":" + (String(e.message).indexOf("deny_paths") >= 0 ? "denied" : "other")); }
                    }
                    return results;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains(".env:denied")
                && out.content.contains("./.env:denied")
                && out.content.contains("sub/../.env:denied"),
            "every spelling must be denied: {}",
            out.content
        );
        // The denial is audited: 3 traced calls, all ok:false.
        let meta = out.meta.expect("audit meta");
        assert_eq!(meta["call_count"], 3, "{meta}");
        assert!(
            meta["calls"]
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c["ok"] == false),
            "{meta}"
        );
    }

    /// 外部评审 P2 跟进：默认 deny_paths 必须拦住密钥目录**本身**（裸目录根，
    /// 无尾 `/`），不止其内容 —— `list_dir` on `.aws`（workdir 根）与 `x/.aws`
    /// （嵌套）都要被拒。本实现的尾段 `**` 跨零段（glob_semantics_vectors 钉住），
    /// `.aws/**` / `**/.aws/**` 直接命中裸目录路径，无需补 `**/.aws` 类条目。
    #[test]
    fn deny_paths_block_listing_secret_dir_root() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join(".aws")).unwrap();
        std::fs::create_dir_all(dir.join("x/.aws")).unwrap();
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"
                    const results = [];
                    for (const p of [".aws", "./.aws", "x/.aws", "x/../.aws"]) {
                        try { tools.call("list_dir", {path: p}); results.push(p + ":ran"); }
                        catch (e) { results.push(p + ":" + (String(e.message).indexOf("deny_paths") >= 0 ? "denied" : "other")); }
                    }
                    return results;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        for want in [
            ".aws:denied",
            "./.aws:denied",
            "x/.aws:denied",
            "x/../.aws:denied",
        ] {
            assert!(
                out.content.contains(want),
                "listing the secret dir root must be denied: {want}\n{}",
                out.content
            );
        }
    }

    /// v2 review — `copy_bytes`: host-streamed byte-exact copy (binary /
    /// invalid-UTF-8 survive without touching a JS string), full policy
    /// surface (fs_write_dirs, deny src, deny dst, outside-workdir dst), and
    /// a REAL audited call.
    #[test]
    fn copy_bytes_is_byte_exact_and_policy_judged() {
        let dir = scratch();
        // 0x00 / 0xFF / invalid-UTF-8 continuation / valid multibyte — bytes
        // that must not silently round-trip through a JS string.
        let blob: Vec<u8> = vec![0x00, 0xFF, 0xC3, 0x28, 0xE4, 0xB8, 0xAD, 0x0A, 0x00];
        std::fs::write(dir.join("blob.bin"), &blob).unwrap();
        std::fs::create_dir_all(dir.join(".aws")).unwrap();
        std::fs::write(dir.join(".aws/creds"), b"AKIA...").unwrap();
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let cc = CodeConfig {
            fs_write_dirs: vec!["out".into()],
            ..CodeConfig::default()
        };
        let out = tool_with(cc)
            .run_with_emit(
                &json!({"code": r#"
                    const r = [];
                    const c = tools.call("copy_bytes", {src: "blob.bin", dst: "out/blob.copy"});
                    r.push("copied:" + c.copied_bytes);
                    const back = tools.call("read_file", {path: "out/blob.copy", raw: true});
                    r.push("bytes:" + back.bytes_read);
                    try { tools.call("copy_bytes", {src: "blob.bin", dst: "leak.bin"}); r.push("leak:ran"); }
                    catch (e) { r.push("leak:" + e.kind); }
                    try { tools.call("copy_bytes", {src: ".aws/creds", dst: "out/x"}); r.push("aws:ran"); }
                    catch (e) { r.push("aws:" + e.kind); }
                    try { tools.call("copy_bytes", {src: "blob.bin", dst: "out/.env"}); r.push("envdst:ran"); }
                    catch (e) { r.push("envdst:" + e.kind); }
                    try { tools.call("copy_bytes", {src: "blob.bin", dst: "../escape.bin"}); r.push("esc:ran"); }
                    catch (e) { r.push("esc:" + e.kind); }
                    return r;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("copied:9"), "{}", out.content);
        assert!(out.content.contains("bytes:9"), "{}", out.content);
        // Host-side byte equality is the ground truth, not the guest's say-so.
        assert_eq!(std::fs::read(dir.join("out/blob.copy")).unwrap(), blob);
        for want in ["leak:denied", "aws:denied", "envdst:denied", "esc:denied"] {
            assert!(
                out.content.contains(want),
                "policy must deny this copy: {want}\n{}",
                out.content
            );
        }
        // The escape attempt left nothing behind.
        assert!(!dir.parent().unwrap().join("escape.bin").exists());
        // 1 ok copy + 1 read-back + 4 refused = 6 audited calls.
        let meta = out.meta.expect("audit meta");
        assert_eq!(meta["call_count"], 6, "{meta}");
    }

    /// Dangling-symlink setup; false where the platform can't test it
    /// (non-unix would need symlink privileges we don't assume). Runtime
    /// bool, not `#[cfg]` + `return` — the latter makes the body below
    /// unreachable and fails `clippy --features code` on Windows with
    /// `-D warnings` (`unreachable_statement`), a gate Linux CI can't see.
    #[cfg(unix)]
    fn create_dangling_symlink(target: &str, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[cfg(not(unix))]
    fn create_dangling_symlink(_target: &str, _link: &Path) -> bool {
        false
    }

    /// 中-1: a dangling symlink write target is refused BEFORE matching —
    /// `ln -s .env bak` then "write bak" must not land on `.env`.
    #[test]
    fn dangling_symlink_write_target_rejected() {
        let dir = scratch();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        if !create_dangling_symlink(".env", &dir.join("bak")) {
            eprintln!("symlink test skipped on this platform");
            return;
        }
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"
                    try { tools.call("write_file", {path: "bak", content: "pwned"}); return "wrote"; }
                    catch (e) { return e.message; }"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("symlink"),
            "the refusal must name the symlink: {}",
            out.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(".env")).unwrap(),
            "SECRET=1\n",
            "the write must NOT have landed on .env"
        );
    }

    /// 中-2: with restrict_to_workdir=false, a READ outside the workdir
    /// passes (deny_paths cannot see it — documented boundary), while a
    /// WRITE outside is always refused (guest writes are bounded by the
    /// workdir; fs_write_dirs is a permission list, not a sandbox mirror).
    #[test]
    fn outside_workdir_read_passes_write_denied() {
        let outer = std::env::temp_dir().join(format!("ta-run-code-out-{}", std::process::id()));
        std::fs::create_dir_all(&outer).unwrap();
        let dir = outer.join("inside");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(outer.join("readable.txt"), "outside-content\n").unwrap();
        let out = tool_with(CodeConfig::default())
            .run_with_emit(
                &json!({"code": r#"
                    const r = tools.call("read_file", {path: "../readable.txt"});
                    let w;
                    try { tools.call("write_file", {path: "../written.txt", content: "x"}); w = "wrote"; }
                    catch (e) { w = e.message; }
                    return {read: r.slice(-10), write: w.slice(0, 60)};"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("e-content"),
            "outside read must pass (the guest shows read_file's tail): {}",
            out.content
        );
        assert!(
            out.content.contains("limited to the working"),
            "outside write must be refused: {}",
            out.content
        );
        assert!(
            !outer.join("written.txt").exists(),
            "the refused write must not have landed"
        );
    }

    /// fs_write_dirs: when non-empty, guest writes must land inside one of
    /// the listed dirs (workdir-relative, prefix containment).
    #[test]
    fn fs_write_dirs_limit_guest_writes() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let cfg = CodeConfig {
            fs_write_dirs: vec!["out".into()],
            ..CodeConfig::default()
        };
        let out = tool_with_tools(cfg, r#"["read_file","write_file"]"#)
            .run_with_emit(
                &json!({"code": r#"
                    let inside, root;
                    try { tools.call("write_file", {path: "out/a.txt", content: "ok"}); inside = "ok"; }
                    catch (e) { inside = "err:" + e.message.slice(0, 30); }
                    try { tools.call("write_file", {path: "root.txt", content: "x"}); root = "wrote"; }
                    catch (e) { root = "denied"; }
                    return {inside, root};"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("\"inside\":\"ok\"")
                && out.content.contains("\"root\":\"denied\""),
            "{}",
            out.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("out/a.txt")).unwrap(),
            "ok"
        );
        assert!(!dir.join("root.txt").exists());
    }

    /// A-8: shell is deliberately OUT of the interception layer's scope —
    /// `cat .env` passes straight through (deny_paths is an anti-misread
    /// guard, not a security boundary; the hostile-guest remedies are
    /// restrict_to_workdir / hooks Deny / dropping shell).
    #[test]
    fn shell_bypasses_deny_paths_documented_boundary() {
        let dir = scratch();
        std::fs::write(dir.join(".env"), "SHELLBYPASS=42\n").unwrap();
        let cfg: Config = serde_json::from_str(
            r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,
                "tools":["shell"]}"#,
        )
        .unwrap();
        let out = RunCodeTool::new(
            &cfg,
            Arc::new(RwLock::new(CodeConfig::default())),
            &reqwest::blocking::Client::new(),
            "k",
            vec![],
            vec![],
        )
        .run_with_emit(
            &json!({"code": r#"return tools.call("shell", {command: "cat .env"}).trim();"#}),
            &ctx(&dir),
            &mut |_| {},
        )
        .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("SHELLBYPASS=42"),
            "shell passes .env through by design: {}",
            out.content
        );
    }

    /// Sec-review M-1: a symlinked INTERMEDIATE component (`out -> somewhere
    /// outside`) must not let a write escape the workdir — the lexical form
    /// says "inside", the open() follows the link outside.
    #[cfg(unix)]
    #[test]
    fn write_through_intermediate_symlink_rejected() {
        let dir = scratch();
        let outside =
            std::env::temp_dir().join(format!("ta-run-code-outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("out")).unwrap();
        let out = tool_with_tools(CodeConfig::default(), r#"["read_file","write_file"]"#)
            .run_with_emit(
                &json!({"code": r#"
                    try { tools.call("write_file", {path: "out/owned.txt", content: "x"}); return {escaped: true}; }
                    catch (e) { return {escaped: false, err: String(e)}; }"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        let ok = serde_json::from_str::<Value>(&out.content).unwrap_or(json!(null));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(ok["escaped"], json!(false), "{}", out.content);
        assert!(
            ok["err"].as_str().unwrap_or("").contains("symlink"),
            "rejection must name the symlink refusal: {}",
            out.content
        );
        assert!(
            !outside.join("owned.txt").exists(),
            "nothing may land outside"
        );
    }

    /// 2026-09 external re-review P1: the symlink walk used to fold `..`
    /// FIRST, which erases the symlink from the path it inspects —
    /// `link/../x` handed the policy layer a clean lexical form ("x",
    /// inside the workdir) while the kernel resolves the actual WRITE
    /// through `link` first and the file lands OUTSIDE. The walk must
    /// follow the ORIGINAL spelling: push-and-check each Normal component,
    /// pop on ParentDir. Also pins the no-false-positive side (`..` past a
    /// real directory stays writable) and the copy_bytes dst arm (same
    /// shared check).
    #[cfg(unix)]
    #[test]
    fn write_via_symlink_dotdot_escape_rejected() {
        let outer = std::env::temp_dir().join(format!("ta-run-code-esc-{}", std::process::id()));
        std::fs::create_dir_all(outer.join("sub")).unwrap();
        let dir = outer.join("inside");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::os::unix::fs::symlink(outer.join("sub"), dir.join("link")).unwrap();
        std::fs::write(dir.join("src.bin"), "payload").unwrap();
        let out = tool_with_tools(CodeConfig::default(), r#"["read_file","write_file"]"#)
            .run_with_emit(
                &json!({"code": r#"
                    let esc, cp, ok;
                    try { tools.call("write_file", {path: "link/../escaped.txt", content: "pwn"}); esc = "wrote"; }
                    catch (e) { esc = String(e); }
                    try { tools.call("copy_bytes", {src: "src.bin", dst: "link/../escaped.bin"}); cp = "copied"; }
                    catch (e) { cp = String(e); }
                    try { tools.call("write_file", {path: "real/../ok.txt", content: "fine"}); ok = "wrote"; }
                    catch (e) { ok = String(e); }
                    return {esc: esc.slice(0, 80), cp: cp.slice(0, 80), ok: ok};"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        let r = serde_json::from_str::<Value>(&out.content).unwrap_or(json!(null));
        assert!(!out.is_error, "{}", out.content);
        assert!(
            r["esc"].as_str().unwrap_or("").contains("symlink"),
            "the write escape must be refused and name the symlink: {}",
            out.content
        );
        assert!(
            r["cp"].as_str().unwrap_or("").contains("symlink"),
            "the copy_bytes dst escape must be refused too: {}",
            out.content
        );
        assert!(
            !outer.join("escaped.txt").exists() && !outer.join("escaped.bin").exists(),
            "nothing may land outside via link/.."
        );
        // No false positive: `..` past a REAL directory component is plain
        // spelling noise, the original-spelling walk must not reject it.
        assert_eq!(
            r["ok"],
            json!("wrote"),
            "real-dir .. must stay writable: {}",
            out.content
        );
        assert!(
            dir.join("ok.txt").exists(),
            "the benign write must have landed"
        );
    }

    /// 2026-09 external re-review P1 (follow-up): an ABSOLUTE spelling whose
    /// literal front is NOT the canonical workdir but which collapses back
    /// inside — `<outside>/../<workdir>/link/../escaped.txt` — used to skip
    /// the symlink walk entirely (the original-spelling strip_prefix failed)
    /// and was then judged "inside" by workdir_relative's pure-lexical
    /// fallback, while the kernel resolved the WRITE through `link` and
    /// landed it outside. Root-anchored walking + deepest-existing-ancestor
    /// resolution close both halves. Also pins the no-false-positive side
    /// (same collapse shape popping only REAL dirs stays writable) and the
    /// `missing/../file` fallback (interception stays out of the way; the
    /// kernel's ENOENT reports it).
    #[cfg(unix)]
    #[test]
    fn write_via_absolute_spelling_collapse_escape_rejected() {
        let outer = std::env::temp_dir().join(format!("ta-run-code-abs-{}", std::process::id()));
        std::fs::create_dir_all(outer.join("sub")).unwrap();
        let dir = outer.join("inside");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(outer.join("sub"), dir.join("link")).unwrap();
        // Absolute spellings must be built from the CANONICAL paths — the
        // policy layer compares against the canonical workdir, so spellings
        // through an unresolved prefix (macOS /var → /private/var) were
        // always "outside" refusals, before and after this fix.
        let dir_canon = dir.canonicalize().unwrap();
        let outer_canon = outer.canonicalize().unwrap();
        // Absolute, literal front outside the workdir, collapses inside —
        // and traverses the symlink mid-spelling. (`sub/..` pops back to the
        // shared parent, then the spelling continues into the workdir —
        // exactly the report's `outside/../work/link/../escaped.txt` shape.)
        let esc = format!(
            "{}/sub/../inside/link/../escaped.txt",
            outer_canon.display()
        );
        // Same collapse shape, but every popped component is a REAL dir.
        let ok = format!("{}/sub/../inside/real_ok.txt", outer_canon.display());
        // A `..` behind a NOT-YET-EXISTING component: canonicalize fails and
        // the ancestor walk bails (file_name of a `..`-terminated path is
        // None) to the lexical fallback — the interception must NOT turn this
        // into a policy refusal; what happens next is the tool's own business
        // (write_file creates parents, so this one lands inside the workdir).
        let hole = format!("{}/missing/../hole.txt", dir_canon.display());
        let code = format!(
            r#"
            let esc, ok, hole;
            try {{ tools.call("write_file", {{path: {esc:?}, content: "pwn"}}); esc = "wrote"; }}
            catch (e) {{ esc = String(e); }}
            try {{ tools.call("write_file", {{path: {ok:?}, content: "fine"}}); ok = "wrote"; }}
            catch (e) {{ ok = String(e); }}
            try {{ tools.call("write_file", {{path: {hole:?}, content: "x"}}); hole = "wrote"; }}
            catch (e) {{ hole = String(e); }}
            return {{esc: esc.slice(0, 120), ok: ok.slice(0, 120), hole: hole.slice(0, 120)}};"#
        );
        let out = tool_with_tools(CodeConfig::default(), r#"["read_file","write_file"]"#)
            .run_with_emit(&json!({"code": code}), &ctx(&dir), &mut |_| {})
            .unwrap();
        let r = serde_json::from_str::<Value>(&out.content).unwrap_or(json!(null));
        assert!(!out.is_error, "{}", out.content);
        assert!(
            r["esc"].as_str().unwrap_or("").contains("symlink"),
            "the absolute-spelling escape must be refused and name the symlink: {}",
            out.content
        );
        assert!(
            !outer.join("escaped.txt").exists(),
            "nothing may land outside via the collapse spelling"
        );
        assert_eq!(
            r["ok"],
            json!("wrote"),
            "real-dir collapse spellings must stay writable: {}",
            out.content
        );
        assert!(dir.join("real_ok.txt").exists(), "the benign write landed");
        let hole = r["hole"].as_str().unwrap_or("");
        assert!(
            !hole.contains("symlink") && !hole.contains("limited to the working"),
            "missing/../file must NOT be a policy refusal (kernel ENOENT instead): {hole}"
        );
        std::fs::remove_dir_all(&outer).ok();
    }

    /// workdir_relative's nonexistent-target fallback must be KERNEL-TRUTH:
    /// `link/../x` resolves THROUGH the symlink (deepest-existing-ancestor
    /// canonicalize), so the judgment is "outside" — the pure-lexical
    /// fallback it replaced called the same spelling "x" (inside).
    /// `missing/../x` keeps the lexical fallback (the ancestor walk cannot
    /// resolve a not-yet-existing component in front of `..`).
    #[cfg(unix)]
    #[test]
    fn workdir_relative_resolves_missing_target_kernel_truthfully() {
        let outer = std::env::temp_dir().join(format!("ta-run-code-wr-{}", std::process::id()));
        std::fs::create_dir_all(outer.join("sub")).unwrap();
        let dir = outer.join("inside");
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::os::unix::fs::symlink(outer.join("sub"), dir.join("link")).unwrap();
        let canon = dir.canonicalize().unwrap();
        assert_eq!(
            workdir_relative(&canon, "link/../x.txt"),
            None,
            "link/../x resolves outside: must be judged outside"
        );
        assert_eq!(
            workdir_relative(&canon, "real/../x.txt"),
            Some("x.txt".to_string()),
            "real-dir .. still collapses inside"
        );
        assert_eq!(
            workdir_relative(&canon, "missing/../x.txt"),
            Some("x.txt".to_string()),
            "missing-component .. keeps the lexical fallback"
        );
        std::fs::remove_dir_all(&outer).ok();
    }

    /// Sec-review M-2: a ~500k-segment guest path is a payload, not a
    /// filename — it must cost neither host stack (recursive glob matcher)
    /// nor time. Reads fall through and fail at the tool; writes are denied.
    #[test]
    fn absurdly_deep_path_is_not_a_recursion_bomb() {
        let deep = format!("{}x", "a/".repeat(60_000));
        let dir = scratch();
        let tool = tool_with_tools(CodeConfig::default(), r#"["read_file","write_file"]"#);
        let read = tool
            .run_with_emit(
                &json!({"code": r#"
                    try { tools.call("read_file", {path: PATH}); return {read: "ok"}; }
                    catch (e) { return {read: "err"}; }"#.replace("PATH", &format!("{deep:?}"))}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        // Completing at all is the assertion (the old recursive matcher
        // aborted the process); the tool itself fails on the absurd path.
        assert!(!read.is_error, "{}", read.content);
        assert!(
            read.content.contains("\"err\""),
            "no real file exists at that path: {}",
            read.content
        );
        let write = tool
            .run_with_emit(
                &json!({"code": r#"
                    try { tools.call("write_file", {path: PATH, content: "x"}); return {write: "ok"}; }
                    catch (e) { return {write: "err"}; }"#.replace("PATH", &format!("{deep:?}"))}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(
            write.content.contains("\"err\""),
            "pathological writes are denied (fail-closed): {}",
            write.content
        );
    }

    /// Write half of the A-8 boundary (the read half — `cat .env` passing
    /// through — is pinned by A-8 itself): the path-interception layer binds
    /// to the 4 file tools (and copy_bytes) ONLY. `shell` is deliberately
    /// un-intercepted (D13: the layer guards model MISREADS, not hostile
    /// guests), so its writes — redirects included — bypass fs_write_dirs and
    /// the write-side symlink refusal. This test PINS that bypass so the
    /// write side is as explicit as the read side; the hostile-guest remedies
    /// stay restrict_to_workdir / hooks Deny / dropping shell from `tools`.
    #[test]
    fn shell_writes_bypass_the_file_tool_write_policy_a8() {
        let dir = scratch();
        // Non-empty fs_write_dirs → file-tool writes are confined to it.
        let code = CodeConfig {
            fs_write_dirs: vec!["allowed".into()],
            ..CodeConfig::default()
        };
        let out = tool_with_tools(code, r#"["read_file","write_file","shell"]"#)
            .run_with_emit(
                &json!({"code": r#"
                    var refused = false;
                    try { tools.call("write_file", {path: "out.txt", content: "x"}); }
                    catch (e) { refused = true; }
                    tools.call("shell", {command: "echo via-shell > out.txt"});
                    return {refused: refused};"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        let ok = serde_json::from_str::<Value>(&out.content).unwrap_or(json!(null));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            ok["refused"],
            json!(true),
            "write_file outside fs_write_dirs must be refused"
        );
        let wrote = std::fs::read_to_string(dir.join("out.txt"));
        assert_eq!(
            wrote.as_deref().unwrap_or_default().trim(),
            "via-shell",
            "shell's redirect write must NOT be intercepted (A-8 write side)"
        );
    }

    /// The task4 scenario, end to end in the guest: raw read → content_b64
    /// write-back preserves EVERY byte (binary payloads included) without any
    /// text-mode round-trip.
    #[test]
    fn guest_raw_copy_is_byte_exact() {
        let dir = scratch();
        let mut bytes: Vec<u8> = b"\xef\xbb\xbfheader,".to_vec();
        bytes.extend((0u8..=255).cycle().take(3000));
        bytes.push(b'\n'); // trailing newline must survive
        std::fs::write(dir.join("src.bin"), &bytes).unwrap();
        let out = tool_with_tools(CodeConfig::default(), r#"["read_file","write_file"]"#)
            .run_with_emit(
                &json!({"code": r#"
                    var copied = 0;
                    for (;;) {
                        var r = tools.call("read_file", {path: "src.bin", raw: true, offset: copied});
                        if (r.bytes_read === 0) break;
                        tools.call("write_file", {path: copied === 0 ? "dst.bin" : "dst.bin.part" + copied, content_b64: r.content_b64});
                        if (!r.truncated) break;
                        copied += r.bytes_read;
                    }
                    return copied;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // Guest ctx has the 2M data-scale budget → one window covers it all.
        assert_eq!(out.content, "0");
        assert_eq!(std::fs::read(dir.join("dst.bin")).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
