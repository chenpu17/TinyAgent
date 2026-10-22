//! The lib family (include/saveLib): lib/INDEX.md grammar, session
//! registration, admission, and the gated host halves of the reserved names.

use super::*;

/// The manifest file the host parses for admission (MD-11): `lib/INDEX.md`,
/// machine lines `<!-- ta-lib: path=… sha256=… needs=… ver=… st=… -->`
/// (path relative to `lib/`). The HOST only ever parses; writes stay with
/// the caller (saveLib upserts via the ordinary file tools — 5.1's
/// counterexample 2 documents that a guest with write access can edit
/// INDEX; the session registration the admission actually trusts is
/// host-side and untouchable).
pub(super) const LIB_INDEX_REL: &str = "lib/INDEX.md";
// ── lib family: include / saveLib (code_memory_design M1) ───────────────────

/// Shared session registration set behind [`HostCtx::lib_sessions`] (see
/// there). `HashSet` over an `Arc<Mutex<_>>` so all clones of one
/// `RunCodeTool` lineage — and every execution they run — see one set.
pub(super) type LibSessions = Arc<std::sync::Mutex<std::collections::HashSet<(String, String)>>>;

/// One parsed `ta-lib` line of `lib/INDEX.md` (MD-11). `needs`/`ver` are
/// carried for forward compatibility (M2+ routing) and round-tripping by
/// saveLib's upsert; M1 admission only consults `sha256` and `st`.
pub(super) struct LibEntry {
    pub(super) sha256: String,
    #[allow(dead_code)] // parsed + round-tripped by saveLib; read from M2 on
    pub(super) needs: String,
    #[allow(dead_code)]
    pub(super) ver: String,
    pub(super) st: String,
}

/// One validated `ta-lib` machine line, fields exactly as written (MD-11).
/// Shared by [`parse_lib_index`] (admission) and [`lib_upsert_index`]
/// (saveLib's merge) so the manifest grammar has ONE parser — external
/// re-review P2: a guest-side near-copy had already drifted (missing the
/// sha/st/ver rules), letting a poisoned line through the write-back guard.
pub(super) struct LibLine {
    pub(super) path: String,
    pub(super) sha256: String,
    pub(super) needs: String,
    pub(super) ver: String,
    pub(super) st: String,
}

/// Validate one `lib/INDEX.md` line (1-based `no` is for error copy).
/// `Ok(None)` = not a machine line (prose/comments are free-form and pass
/// through untouched). Same strictness both directions: a malformed line is
/// a HARD error whether it is being read for admission or merged by saveLib.
pub(super) fn parse_lib_line(no: usize, raw: &str) -> Result<Option<LibLine>, String> {
    let line = raw.trim();
    if !(line.starts_with("<!-- ta-lib:") && line.ends_with("-->")) {
        return Ok(None);
    }
    let body = line
        .strip_prefix("<!-- ta-lib:")
        .and_then(|s| s.strip_suffix("-->"))
        .unwrap_or("");
    let mut path: Option<String> = None;
    let mut sha256: Option<String> = None;
    let mut needs: Option<String> = None;
    let mut ver: Option<String> = None;
    let mut st: Option<String> = None;
    for tok in body.split_whitespace() {
        let (k, v) = tok.split_once('=').unwrap_or(("", ""));
        let v = v.to_string();
        match k {
            "path" if path.is_none() => path = Some(v),
            "sha256" if sha256.is_none() => sha256 = Some(v),
            "needs" if needs.is_none() => needs = Some(v),
            "ver" if ver.is_none() => ver = Some(v),
            "st" if st.is_none() => st = Some(v),
            _ => {
                return Err(format!(
                    "line {}: unknown or duplicated field {:?}",
                    no,
                    if k.is_empty() { tok } else { k }
                ))
            }
        }
    }
    let (Some(path), Some(sha256), Some(needs), Some(ver), Some(st)) =
        (path, sha256, needs, ver, st)
    else {
        return Err(format!(
            "line {}: a ta-lib line needs all five fields \
             (path/sha256/needs/ver/st)",
            no
        ));
    };
    if path.is_empty()
        || path.starts_with('/')
        || path.contains("..")
        || path.split('/').any(|s| s.is_empty())
    {
        return Err(format!(
            "line {}: path must be a clean lib/-relative path",
            no
        ));
    }
    if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("line {}: sha256 must be 64 hex chars", no));
    }
    if st != "ok" && st != "draft" {
        return Err(format!("line {}: st must be ok or draft, got {st:?}", no));
    }
    if ver.is_empty() || !ver.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("line {}: ver must be a number, got {ver:?}", no));
    }
    Ok(Some(LibLine {
        path,
        sha256,
        needs,
        ver,
        st,
    }))
}

/// Parse the manifest (MD-11). Strict by design — a malformed, duplicated or
/// conflicting entry is a HARD error, never a silent pick: INDEX is the
/// cross-session trust list, and "which entry won" is exactly the ambiguity
/// the design refuses. A missing file is the caller's case (→ "no entry"
/// admission error), not a parse error.
pub(super) fn parse_lib_index(
    text: &str,
) -> Result<std::collections::HashMap<String, LibEntry>, String> {
    let mut map: std::collections::HashMap<String, LibEntry> = std::collections::HashMap::new();
    for (idx, raw) in text.lines().enumerate() {
        let Some(LibLine {
            path,
            sha256,
            needs,
            ver,
            st,
        }) = parse_lib_line(idx + 1, raw)?
        else {
            continue;
        };
        if map
            .insert(
                path.clone(),
                LibEntry {
                    sha256,
                    needs,
                    ver,
                    st,
                },
            )
            .is_some()
        {
            return Err(format!(
                "line {}: duplicate ta-lib entry for path {path:?}",
                idx + 1
            ));
        }
    }
    Ok(map)
}

/// saveLib's INDEX merge (MD-10, host side since external re-review P1/P2):
/// validate `text` with the SAME line parser the admission uses, then upsert
/// the `(rel, sha256)` entry — in-place replacement of the same-path line
/// (preserving its `needs`/`ver`), else append at the end. Pure function:
/// text in, merged text out; the caller does the IO through the ordinary
/// gated file tools. Output is byte-identical to the merge the prelude used
/// to do in JS (the format-pinning tests hold it to that).
pub(super) fn lib_upsert_index(rel: &str, sha256: &str, text: &str) -> Result<String, String> {
    // The registration key must satisfy the same grammar a parsed line's
    // path does — plus no whitespace (machine lines are whitespace-tokenized;
    // the prelude rejects whitespace spellings first, this is the host-side
    // backstop for direct reserved-name callers).
    if rel.is_empty()
        || rel.starts_with('/')
        || rel.contains("..")
        || rel.split('/').any(|s| s.is_empty())
        || rel.chars().any(|c| c.is_whitespace())
    {
        return Err(format!(
            "saveLib: registration key must be a clean lib/-relative path, got {rel:?}"
        ));
    }
    if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "saveLib: registration digest must be 64 hex chars, got {sha256:?}"
        ));
    }
    let mut out: Vec<String> = Vec::new();
    let mut needs = "-".to_string();
    let mut ver = "1".to_string();
    let mut hit = false;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (idx, line) in text.lines().enumerate() {
        let parsed = parse_lib_line(idx + 1, line)?;
        if let Some(entry) = &parsed {
            if !seen.insert(entry.path.clone()) {
                return Err(format!(
                    "lib/INDEX.md line {}: duplicate ta-lib entry for path {:?} — \
                     fix the index before saving",
                    idx + 1,
                    entry.path
                ));
            }
            if entry.path == rel {
                // Preserve the existing entry's metadata; st goes back to
                // draft (the content just changed, approvals reset — MD-10).
                needs = entry.needs.clone();
                ver = entry.ver.clone();
                out.push(format!(
                    "<!-- ta-lib: path={rel} sha256={sha256} needs={needs} ver={ver} st=draft -->"
                ));
                hit = true;
                continue;
            }
        }
        out.push(line.to_string());
    }
    if !hit {
        if !out.is_empty() && out.last().is_some_and(|l| !l.is_empty()) {
            out.push(String::new());
        }
        out.push(format!(
            "<!-- ta-lib: path={rel} sha256={sha256} needs={needs} ver={ver} st=draft -->"
        ));
    }
    let mut merged = out.join("\n");
    merged.push('\n');
    Ok(merged)
}

/// Full-hex sha256 of delivered bytes (MD-12 — hashed on what the guest will
/// actually receive, i.e. the lossy-decoded source, not the raw disk bytes:
/// invalid-UTF-8 files hash as they deliver).
pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = sha2::Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// Confine a lib-family path: the workdir-relative normalized form, required
/// to sit under `lib/` (MD-2 ①). Symlink safety comes free from
/// [`workdir_relative`]'s canonicalize-based walk — `lib/x.js` symlinked
/// outside the workdir canonicalizes outside and yields `None`; absolute
/// spellings resolve from the workdir anchor like every other guest path.
/// Whitespace is refused too: `lib/INDEX.md` machine lines are
/// whitespace-tokenized, so a whitespace path can never round-trip through
/// the manifest — admitting it would let saveLib poison the whole channel.
pub(super) fn lib_rel(workdir_canon: &Path, path: &str) -> Option<String> {
    let rel = workdir_relative(workdir_canon, path)?;
    (rel.starts_with("lib/")
        && rel.len() > "lib/".len()
        && !rel.ends_with('/')
        && !rel.chars().any(char::is_whitespace))
    .then_some(rel)
}

/// Accept the bare spelling saveLib's own return value uses. The wire form is
/// `lib/…`, but saveLib's return strips the prefix for display ("util.js") —
/// and a guest that fed that value straight back into include got the
/// confinement refusal, sending the model on a prefix-fixing detour EVERY
/// time it trusted the API's own output (observed live twice with different
/// models). Normalize: a relative name with no prefix, no backslash, no `..`,
/// and no whitespace is read as `lib/{name}`. Nothing outside lib/ becomes
/// reachable — confinement ([`lib_rel`]) is unchanged; this only widens the
/// ACCEPTED SPELLING of the same file.
pub(super) fn lib_path_normalize(path: &str) -> String {
    let bare = !path.starts_with("lib/")
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains("..")
        && !path.contains(':')
        && !path.chars().any(char::is_whitespace);
    if bare {
        format!("lib/{path}")
    } else {
        path.to_string()
    }
}

/// Dispatch for the lib-family reserved names — the Phase-B-shaped sequence
/// with the lib policy layers first (MD-2 order: ① lib/ confinement
/// ② deny_paths ③ hooks gate as pseudo-tool ④ trace) and the HOST doing the
/// work instead of `run_tool`. Emits the event pair, audits into `calls`,
/// counts against `max_tool_calls` (a probing guest pays like any caller).
/// Gate off (`code.include=false`): explicit refusal naming the knob, still
/// audited — the pre-guest contract of the old binary minus the probe.
pub(super) fn lib_reserved_call(caller: &mut Caller<'_, HostCtx>, name: &str, args: &Value) -> i64 {
    let started = Instant::now();
    let (in_len, in_hash) = hash_input(args);
    // The pseudo-tool name the hooks gate / after_tool see ("include" /
    // "hash_file" — MD-2 ③): dunder names are the wire protocol, the
    // policy surface is the tool the action actually is.
    let pseudo = name.trim_start_matches("__");
    // Dual-hash audit (§8.4): the hooks-Modify effective input's hash, set
    // only when a Modify actually changed the input (external audit P1-2 —
    // the lib family used to treat Modify as Allow and left this always
    // None).
    let mut in_hash_eff: Option<String> = None;
    let (output, class) = {
        let d = caller.data();
        // SAFETY: same frame-limited alias as in `host_call_tool`.
        let emit = unsafe { &mut *d.emit };
        emit(AgentEvent::ToolCall {
            name: name.to_string(),
            input: args.clone(),
        });
        if !d.snap.include {
            (
                ToolOutput::error(format!(
                    "run_code: {name} is not available in this configuration \
                     (code.include=false) — the lib helpers are disabled"
                )),
                "denied",
            )
        } else {
            let path = args.get("path").and_then(Value::as_str);
            let confined = path.and_then(|p| lib_rel(&d.workdir_canon, &lib_path_normalize(p)));
            // ① confinement + ② deny_paths — ahead of the hooks gate: a path
            // outside the lib root is a guest bug, not a policy question.
            let policy_err = match (&path, &confined) {
                (None, _) => Some("run_code: lib helpers need a string field 'path'".to_string()),
                (Some(p), None) => Some(format!(
                    "run_code: lib helpers only accept relative paths under lib/ \
                     ('{p}' is not one)"
                )),
                (Some(p), Some(rel)) => {
                    if deny_match(&d.snap.deny_paths, rel) {
                        Some(format!(
                            "run_code: path '{p}' matches a code.deny_paths pattern"
                        ))
                    } else {
                        None
                    }
                }
            };
            match policy_err {
                Some(err) => (ToolOutput::error(err), "denied"),
                None => match gate_tool_call(&d.ctx, pseudo, args) {
                    ToolDecision::Deny(mut reason) => {
                        crate::tools::truncate(&mut reason, d.ctx.max_output_chars);
                        let output = ToolOutput::error(reason);
                        notify_tool_after(&d.ctx, pseudo, args, &output, false);
                        (output, "hooks")
                    }
                    decision => {
                        // Hooks Modify (external audit P1-2): the lib family
                        // used to treat Modify as Allow and execute the
                        // ORIGINAL path — a host redirecting lib/unsafe.js to
                        // lib/safe.js still got unsafe. Mirror
                        // `host_call_tool`'s shape: re-run the policy layers
                        // (① confinement ② deny_paths) on the EFFECTIVE
                        // path; a modified path that fails them is a
                        // refusal (deny/fs semantics — after_tool does not
                        // fire), never a silent fallback to the original.
                        let effective: Value = match &decision {
                            ToolDecision::Modify(v) => v.clone(),
                            _ => args.clone(),
                        };
                        let eff_path = effective.get("path").and_then(Value::as_str);
                        let eff_confined = eff_path
                            .and_then(|p| lib_rel(&d.workdir_canon, &lib_path_normalize(p)));
                        let eff_err = match (&eff_path, &eff_confined) {
                            (Some(p), Some(rel)) if !deny_match(&d.snap.deny_paths, rel) => None,
                            (Some(p), _) => Some(format!(
                                "run_code: hooks Modify replaced the path with '{p}', \
                                 which is not an admissible lib path (relative under \
                                 lib/, not matching code.deny_paths) — refusing rather \
                                 than falling back to the original"
                            )),
                            (None, _) => Some(
                                "run_code: hooks Modify dropped the string 'path' field \
                                 the lib helpers need — refusing rather than falling \
                                 back to the original"
                                    .to_string(),
                            ),
                        };
                        match eff_err {
                            Some(err) => (ToolOutput::error(err), "denied"),
                            None => {
                                if ctx_cancelled(&d.ctx) {
                                    let output = ToolOutput::error(CANCEL_HALTED_WHY.to_string());
                                    notify_tool_after(&d.ctx, pseudo, args, &output, false);
                                    (output, "cancel")
                                } else {
                                    if effective != *args {
                                        in_hash_eff = Some(hash_input(&effective).1);
                                    }
                                    let rel = eff_confined.expect("eff_err None implies confined");
                                    let eff_path = eff_path.expect("eff_err None implies path");
                                    let (output, class) = if name == INCLUDE_NAME {
                                        include_deliver(d, &rel, eff_path)
                                    } else {
                                        hash_file_register(d, &rel)
                                    };
                                    notify_tool_after(&d.ctx, pseudo, args, &output, true);
                                    (output, class)
                                }
                            }
                        }
                    }
                },
            }
        }
    };
    emit_inner_result(caller, name, &output);
    {
        let d = caller.data_mut();
        // ④ trace (MD-2): the wire name — audit records what the guest
        // actually called; `in_hash_eff` records the hooks-Modify effective
        // input when it differed (dual-hash discipline, host_call_tool
        // parity).
        d.calls.push(CallTrace {
            name: name.to_string(),
            ok: !output.is_error,
            ms: started.elapsed().as_millis(),
            in_len,
            in_hash,
            in_hash_eff,
        });
    }
    if output.is_error {
        envelope_err(caller, class, Some(name), Some(false), &output.content)
    } else {
        envelope_or_trap(caller, true, &output.content)
    }
}

/// Read a lib file with the HOST-side allocation capped at the lib read
/// limit: `take(MAX+1)` bounds the buffer even if the file is bigger on disk
/// (or grows mid-read). The wasm memory limiter cannot see host allocations,
/// so an unbounded slurp here would be an OOM side channel the sandbox
/// budgets know nothing about — `read_file` streams; this is the lib
/// channel's equivalent discipline.
pub(super) fn read_lib_capped(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    file.take(MAX_CODE_BYTES as u64 + 1).read_to_end(&mut buf)?;
    Ok(buf)
}

/// `__include` (MD-14 admission): read the lib file, hash the DELIVERED
/// bytes, verify against the manifest entry, then admit on `st=ok` OR a
/// this-session registration. Every failure is a hard "lib" error — the
/// integrity chain is the feature; nothing degrades to "include anyway".
pub(super) fn include_deliver(
    d: &HostCtx,
    rel: &str,
    spelling: &str,
) -> (ToolOutput, &'static str) {
    let err = |msg: String| (ToolOutput::error(msg), "lib");
    let manifest_rel = rel.strip_prefix("lib/").unwrap_or(rel);
    let bytes = match read_lib_capped(&d.workdir_canon.join(rel)) {
        Ok(b) => b,
        Err(e) => {
            return err(format!(
                "run_code: cannot read '{spelling}' for include: {e} — saveLib it first"
            ))
        }
    };
    if bytes.len() > MAX_CODE_BYTES {
        return err(format!(
            "run_code: '{spelling}' is {} bytes, over the {}-byte include read cap \
             — split the library (a lib file shares the size budget of one \
             run_code program)",
            bytes.len(),
            MAX_CODE_BYTES
        ));
    }
    let src = String::from_utf8_lossy(&bytes).into_owned();
    let hex = sha256_hex(src.as_bytes());
    let index_text = match std::fs::read_to_string(d.workdir_canon.join(LIB_INDEX_REL)) {
        Ok(t) => t,
        // A missing index is the legitimate "nothing registered yet" case.
        // Any OTHER IO error (permissions, a directory in the way…) is
        // reported as what it is — the old `unwrap_or_default()` folded it
        // into "no entry", sending the guest to saveLib for an index it
        // cannot even read (external re-review P3; same triage as
        // `__lib_index_state`).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return err(format!("run_code: cannot read {LIB_INDEX_REL}: {e}")),
    };
    let index = match parse_lib_index(&index_text) {
        Ok(m) => m,
        Err(e) => return err(format!("run_code: {LIB_INDEX_REL} is malformed: {e}")),
    };
    let Some(entry) = index.get(manifest_rel) else {
        return err(format!(
            "run_code: '{spelling}' has no entry in {LIB_INDEX_REL} — register it \
             first with saveLib(\"{rel}\", src), then include it"
        ));
    };
    if entry.sha256 != hex {
        return err(format!(
            "run_code: '{spelling}' on disk does not match its {LIB_INDEX_REL} entry \
             (file sha256:{}…, registered sha256:{}…) — the library changed after \
             registration; re-run saveLib or restore the file",
            &hex[..16],
            &entry.sha256[..16]
        ));
    }
    let registered = d
        .lib_sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&(rel.to_string(), hex.clone()));
    if entry.st != "ok" && !registered {
        return err(format!(
            "run_code: '{spelling}' is st=draft and is not registered in this session — \
             admission refused. If saveLib succeeded for it earlier in this conversation, \
             the session registration was probably cleared by an app restart: re-run saveLib \
             with the identical content to re-register (same hash, review state untouched). \
             Otherwise the library needs a human review and promotion to st=ok before it \
             works across sessions."
        ));
    }
    // The source rides inside an object on purpose (same shape as
    // `__lib_update_index`'s {"index": …}): the guest glue auto-JSON-parses
    // envelope content, and a library whose WHOLE text parses as JSON (a
    // data-file lib) would arrive as an object — `(0, eval)(object)` is a
    // silent no-op, the include "succeeds" while loading nothing (external
    // re-review P3). Wrapped, the parse lands on the envelope object and
    // the source always arrives as the string it is.
    (ToolOutput::text(json!({ "src": src }).to_string()), "tool")
}

/// `__hash_file` (MD-12/MD-14): hash the delivered bytes of a lib file and
/// REGISTER `(canonical rel path, sha256)` in the session set. This is the
/// host-side half of saveLib — the registration the admission actually
/// trusts, derived from file facts, never from guest self-report.
pub(super) fn hash_file_register(d: &HostCtx, rel: &str) -> (ToolOutput, &'static str) {
    let bytes = match read_lib_capped(&d.workdir_canon.join(rel)) {
        Ok(b) => b,
        Err(e) => {
            return (
                ToolOutput::error(format!("run_code: cannot read '{rel}': {e}")),
                "lib",
            )
        }
    };
    if bytes.len() > MAX_CODE_BYTES {
        return (
            ToolOutput::error(format!(
                "run_code: '{rel}' is {} bytes, over the {}-byte lib read cap — \
                 split the library",
                bytes.len(),
                MAX_CODE_BYTES
            )),
            "lib",
        );
    }
    let src = String::from_utf8_lossy(&bytes).into_owned();
    let hex = sha256_hex(src.as_bytes());
    d.lib_sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((rel.to_string(), hex.clone()));
    (
        // `rel` (external audit P2-6): the CANONICAL workdir-relative path
        // the registration was keyed under — saveLib's manifest entry must
        // use this spelling, never a raw `path.slice(4)` of the guest's
        // spelling ("lib/./x.js" used to write an entry the parser
        // rejects, poisoning the whole channel).
        ToolOutput::text(json!({ "sha256": hex, "bytes": src.len(), "rel": rel }).to_string()),
        "tool",
    )
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── code_memory M1: lib family (include / saveLib) ─────────────────────
    //
    // A-M 编号对应 docs/code_memory_design.md §7 的验收清单（A-M8 属 M3）。

    /// [`include_cfg`] + write access (saveLib writes the lib file AND the
    /// INDEX upsert through the ordinary file tools).
    fn lib_tool() -> RunCodeTool {
        tool_with_tools(include_cfg(), r#"["read_file","write_file","list_dir"]"#)
    }

    /// Hand-write a lib + a matching st=ok INDEX entry Rust-side (the
    /// "human-reviewed library" leg — no session registration involved).
    fn plant_lib(dir: &std::path::Path, rel: &str, src: &str, st: &str) -> String {
        let lib_rel = format!("lib/{rel}");
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join(&lib_rel), src).unwrap();
        let hex = sha256_hex(src.as_bytes());
        std::fs::write(
            dir.join(LIB_INDEX_REL),
            format!(
                "# lib index\n<!-- ta-lib: path={rel} sha256={hex} needs=- ver=1 st={st} -->\n"
            ),
        )
        .unwrap();
        hex
    }

    /// A-M1: saveLib → include in the same execution; idempotent double
    /// include (one load); `__hash_file` direct call returns a full digest;
    /// audit traces both reserved names.
    #[test]
    fn lib_save_and_include_same_execution() {
        let dir = scratch();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            saveLib("lib/c.js", "globalThis.loads = (globalThis.loads || 0) + 1; function add(a, b) { return a + b; }");
            include("lib/c.js");
            include("lib/c.js");
            var h = tools.call("__hash_file", {path: "lib/c.js"});
            return {sum: add(2, 3), loads: globalThis.loads, hashlen: h.sha256.length};"#,
        );
        assert_eq!(v["sum"], json!(5), "included lib is callable: {v}");
        assert_eq!(v["loads"], json!(1), "double include loads once: {v}");
        assert_eq!(v["hashlen"], json!(64), "full sha256 hex: {v}");
        // The manifest on disk carries the machine line with st=draft (MD-10).
        let index = std::fs::read_to_string(dir.join(LIB_INDEX_REL)).unwrap();
        assert!(index.contains("path=c.js"), "index: {index}");
        assert!(
            index.contains("st=draft"),
            "saveLib never ok's itself: {index}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M1/A-M7 + MD-14 clone-sharing: a draft lib is admitted in a LATER
    /// execution of the SAME tool lineage (session registration survives the
    /// execution), and refused for a FRESH lineage (session is per-lineage,
    /// never persisted).
    #[test]
    fn lib_draft_admission_is_session_scoped() {
        let dir = scratch();
        let tool = lib_tool();
        let out = tool
            .run_with_emit(
                &json!({"code": r#"saveLib("lib/c.js", "function add(a, b) { return a + b; }"); return 1;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // Same lineage, new execution: admitted via session registration.
        let v = js_out(&tool, &dir, r#"include("lib/c.js"); return add(9, 1);"#);
        assert_eq!(v, json!(10), "session-registered draft admits: {v}");
        // Fresh lineage, same disk state: draft without session → refused.
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            try { include("lib/c.js"); return {refused: false}; }
            catch (e) { return {refused: true, kind: e.kind, msg: String(e)}; }"#,
        );
        assert_eq!(v["refused"], json!(true), "{v}");
        assert_eq!(v["kind"], json!("lib"), "{v}");
        assert!(
            v["msg"].as_str().unwrap_or("").contains("st=draft"),
            "error names the admission state: {v}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M7 st=ok leg: a hand-reviewed INDEX (st=ok, matching hash) admits
    /// with NO session registration — the cross-session path.
    #[test]
    fn lib_st_ok_admits_without_session() {
        let dir = scratch();
        plant_lib(&dir, "t.js", "function twice(x) { return x * 2; }", "ok");
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"include("lib/t.js"); return twice(21);"#,
        );
        assert_eq!(v, json!(42), "{v}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M6: content changed after registration → hash mismatch is a hard
    /// refusal even though the session registered the OLD bytes.
    #[test]
    fn lib_tamper_after_registration_rejected() {
        let dir = scratch();
        let tool = lib_tool();
        let out = tool
            .run_with_emit(
                &json!({"code": r#"saveLib("lib/c.js", "function f() { return 1; }"); return 1;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        std::fs::write(dir.join("lib/c.js"), "function f() { return 2; }").unwrap();
        let v = js_out(
            &tool,
            &dir,
            r#"
            try { include("lib/c.js"); return {refused: false}; }
            catch (e) { return {refused: true, kind: e.kind, msg: String(e)}; }"#,
        );
        assert_eq!(v["refused"], json!(true), "{v}");
        assert_eq!(v["kind"], json!("lib"), "{v}");
        assert!(
            v["msg"].as_str().unwrap_or("").contains("does not match"),
            "integrity error: {v}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M10: an unregistered file (no INDEX entry at all) is refused with
    /// guidance toward saveLib.
    #[test]
    fn lib_missing_entry_rejected() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/x.js"), "var x = 1;").unwrap();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            try { include("lib/x.js"); return {refused: false}; }
            catch (e) { return {refused: true, msg: String(e)}; }"#,
        );
        assert_eq!(v["refused"], json!(true), "{v}");
        assert!(
            v["msg"]
                .as_str()
                .unwrap_or("")
                .contains("no entry in lib/INDEX.md"),
            "{v}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// saveLib 的返回值剥掉 lib/ 前缀（展示形态）——把该值原样喂回 include
    /// 曾经撞上收口拒绝，模型每次信 API 自己的输出都要绕一圈补前缀（实测
    /// 两个不同模型各栽一次）。裸拼写必须解析到与前缀拼写同一个文件。
    #[test]
    fn lib_bare_path_spelling_accepted() {
        let dir = scratch();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            saveLib("lib/bare.js", "globalThis.bareVal = 7;");
            include("lib/bare.js");
            include("bare.js");
            return {bare: globalThis.bareVal};"#,
        );
        // 两种拼写都不抛（抛了 js_out 会把错误作为返回值，字段缺失）
        assert_eq!(v["bare"], json!(7), "{v}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// draft 且本会话未登记的拒绝文案必须说实话：「本会话未登记」+ 重启
    /// 清登记的可能性 + 原样重存的自愈路径；不再教人手改 lib/INDEX.md
    /// （晋升入口由宿主引导讲，内核不指定 HOW）。保住 "admission refused"
    /// 标记短语——宿主引导按它识别这条报错。
    #[test]
    fn lib_draft_refusal_names_restart_path_and_skips_hand_edit_teaching() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/x.js"), "var x = 1;").unwrap();
        let hex = sha256_hex(b"var x = 1;");
        std::fs::write(
            dir.join(LIB_INDEX_REL),
            format!("<!-- ta-lib: path=x.js sha256={hex} needs=- ver=1 st=draft -->\n"),
        )
        .unwrap();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            try { include("lib/x.js"); return {refused: false}; }
            catch (e) { return {refused: true, msg: String(e)}; }"#,
        );
        assert_eq!(v["refused"], json!(true), "{v}");
        let msg = v["msg"].as_str().unwrap_or("");
        assert!(msg.contains("admission refused"), "{msg}");
        assert!(msg.contains("app restart"), "{msg}");
        assert!(msg.contains("re-run saveLib"), "{msg}");
        assert!(!msg.contains("set st=ok in"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M11: duplicate/conflicting INDEX entries are a hard error — no
    /// silent pick between the lines.
    #[test]
    fn lib_duplicate_index_entries_rejected() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/x.js"), "var x = 1;").unwrap();
        let hex = sha256_hex(b"var x = 1;");
        let other = sha256_hex(b"var x = 2;");
        std::fs::write(
            dir.join(LIB_INDEX_REL),
            format!(
                "<!-- ta-lib: path=x.js sha256={hex} needs=- ver=1 st=ok -->\n\
                 <!-- ta-lib: path=x.js sha256={other} needs=- ver=1 st=ok -->\n"
            ),
        )
        .unwrap();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            try { include("lib/x.js"); return {refused: false}; }
            catch (e) { return {refused: true, msg: String(e)}; }"#,
        );
        assert_eq!(v["refused"], json!(true), "{v}");
        assert!(v["msg"].as_str().unwrap_or("").contains("duplicate"), "{v}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M12: the host-side source-read bound — an oversized lib fails with
    /// the cap named, before any admission judgment.
    #[test]
    fn lib_read_cap_error() {
        let dir = scratch();
        let big = "x".repeat(MAX_CODE_BYTES + 1);
        plant_lib(&dir, "big.js", &big, "ok");
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            try { include("lib/big.js"); return {refused: false}; }
            catch (e) { return {refused: true, kind: e.kind, msg: String(e)}; }"#,
        );
        assert_eq!(v["refused"], json!(true), "{v}");
        assert_eq!(v["kind"], json!("lib"), "{v}");
        assert!(v["msg"].as_str().unwrap_or("").contains("read cap"), "{v}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M2: cycle-cut (a → b → a loads each once) and the throwing-eval
    /// marker discipline (marker deleted on throw → a retry re-attempts).
    #[test]
    fn lib_cycle_cut_and_retry_after_throw() {
        let dir = scratch();
        let tool = lib_tool();
        let out = tool
            .run_with_emit(
                &json!({"code": r#"
                    saveLib("lib/a.js", "include('lib/b.js'); globalThis.a = (globalThis.a || 0) + 1;");
                    saveLib("lib/b.js", "include('lib/a.js'); globalThis.b = (globalThis.b || 0) + 1;");
                    return 1;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v = js_out(
            &tool,
            &dir,
            r#"
            include("lib/a.js");
            return {a: globalThis.a, b: globalThis.b};"#,
        );
        assert_eq!(v, json!({"a": 1, "b": 1}), "cycle cut, each lib once: {v}");

        let v = js_out(
            &tool,
            &dir,
            r#"
            saveLib("lib/x.js", "throw new Error('boom');");
            var caught = 0, rethrown = 0;
            try { include("lib/x.js"); } catch (e) { caught = 1; }
            try { include("lib/x.js"); rethrown = 1; } catch (e2) { rethrown = String(e2).indexOf("boom") !== -1 ? 2 : 3; }
            return {caught: caught, second: rethrown};"#,
        );
        assert_eq!(
            v,
            json!({"caught": 1, "second": 2}),
            "throwing lib re-attempts after the marker was deleted: {v}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M3: a hooks Deny on the pseudo-tool ("include" — no dunder) throws
    /// kind="hooks" in the guest, fires after_tool(executed=false), and
    /// audits as not-ok.
    #[test]
    fn lib_hooks_deny_pseudo_tool() {
        use crate::hooks::{Hooks, ToolDecision};
        use std::sync::Mutex;

        struct DenyIncludeHooks {
            afters: Mutex<Vec<bool>>,
        }
        impl Hooks for DenyIncludeHooks {
            fn before_tool(&self, tool: &str, _i: &Value, _c: &ToolCtx) -> ToolDecision {
                if tool == "include" {
                    ToolDecision::Deny("no libs today".into())
                } else {
                    ToolDecision::Allow
                }
            }
            fn after_tool(
                &self,
                _t: &str,
                _i: &Value,
                _o: &ToolOutput,
                executed: bool,
                _ctx: &ToolCtx,
            ) {
                self.afters.lock().unwrap().push(executed);
            }
        }
        let hooks = Arc::new(DenyIncludeHooks {
            afters: Mutex::new(Vec::new()),
        });
        let dir = scratch();
        plant_lib(&dir, "t.js", "var t = 1;", "ok");
        let mut c = ctx(&dir);
        c.hooks = Some(hooks.clone());
        let out = lib_tool()
            .run_with_emit(
                &json!({"code": r#"
                    try { include("lib/t.js"); return {denied: false}; }
                    catch (e) { return {denied: true, kind: e.kind, msg: String(e)}; }"#}),
                &c,
                &mut |_| {},
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["denied"], json!(true), "{}", out.content);
        assert_eq!(v["kind"], json!("hooks"), "{}", out.content);
        assert!(
            v["msg"].as_str().unwrap_or("").contains("no libs today"),
            "{}",
            out.content
        );
        assert_eq!(*hooks.afters.lock().unwrap(), vec![false]);
        let meta = out.meta.expect("audit meta");
        assert_eq!(meta["calls"][0]["name"], json!("__include"));
        assert_eq!(meta["calls"][0]["ok"], json!(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M5: deny_paths covers the lib channel — `lib/**` refuses an
    /// otherwise-valid, st=ok library.
    #[test]
    fn lib_deny_paths_blocks_include() {
        let dir = scratch();
        plant_lib(&dir, "t.js", "var t = 1;", "ok");
        let cfg = CodeConfig {
            deny_paths: vec!["lib/**".into()],
            ..include_cfg()
        };
        let out = tool_with_tools(cfg, r#"["read_file","write_file","list_dir"]"#)
            .run_with_emit(
                &json!({"code": r#"
                    try { include("lib/t.js"); return {denied: false}; }
                    catch (e) { return {denied: true, kind: e.kind, msg: String(e)}; }"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["denied"], json!(true), "{}", out.content);
        assert_eq!(v["kind"], json!("denied"), "{}", out.content);
        assert!(
            v["msg"].as_str().unwrap_or("").contains("deny_paths"),
            "{}",
            out.content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M9 escape legs on the lib channel: `../` spelling escapes and
    /// symlinks under lib/ must be refused by the confinement walk (the same
    /// three-discipline walk the file tools use), for BOTH `__include` and
    /// `__hash_file`. The symlink leg forges a matching-hash INDEX entry —
    /// proving the refusal is the path walk, not a hash/admission miss.
    #[cfg(unix)]
    #[test]
    fn lib_channel_path_escape_legs() {
        use std::os::unix::fs::symlink;
        let dir = scratch();
        plant_lib(&dir, "real.js", "globalThis.fromReal = 1;", "ok");
        // A file OUTSIDE lib/ that the escapes try to reach; give it a valid
        // machine line so only the path walk stands in the way.
        std::fs::write(dir.join("evil.js"), "globalThis.escaped = 1;").unwrap();
        symlink("../evil.js", dir.join("lib/alias.js")).unwrap();
        let hex = sha256_hex(b"globalThis.escaped = 1;");
        let index = std::fs::read_to_string(dir.join(LIB_INDEX_REL)).unwrap();
        std::fs::write(
            dir.join(LIB_INDEX_REL),
            format!("{index}<!-- ta-lib: path=alias.js sha256={hex} needs=- ver=1 st=ok -->\n"),
        )
        .unwrap();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            function attempt(kind, path) {
                try { tools.call(kind, {path: path}); return "ran"; }
                catch (e) { return e.kind + ": " + String(e); }
            }
            return {
                dotsInclude: attempt("__include", "lib/../evil.js"),
                dotsHash: attempt("__hash_file", "lib/../evil.js"),
                linkInclude: attempt("__include", "lib/alias.js"),
                linkHash: attempt("__hash_file", "lib/alias.js"),
                clean: (include("lib/real.js"), globalThis.fromReal === 1),
            };"#,
        );
        for leg in ["dotsInclude", "dotsHash", "linkInclude", "linkHash"] {
            let msg = v[leg].as_str().unwrap_or_default();
            assert!(
                msg.starts_with("denied: ") && msg.contains("under lib/"),
                "{leg} must refuse at the confinement walk: {v}"
            );
        }
        assert_eq!(v["clean"], json!(true), "the honest path still works: {v}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whitespace lib paths (impl-review #3): INDEX machine lines are
    /// whitespace-tokenized on the host — `saveLib("lib/a b.js")` used to
    /// sail through and poison EVERY later include with a malformed-index
    /// error. Rejected at the JS boundary before any write, and the host
    /// channel (`lib_rel`) refuses such paths even when a file exists.
    /// Plus the `__hash_file` read-cap leg: a >1 MiB lib file refuses with
    /// the cap error instead of an unbounded host slurp (impl-review #4).
    #[test]
    fn lib_whitespace_paths_refused_and_hash_read_capped() {
        let dir = scratch();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            var saveMsg = null;
            try { saveLib("lib/a b.js", "var x = 1;"); } catch (e) { saveMsg = String(e); }
            var hashKind = null;
            try { tools.call("__hash_file", {path: "lib/a b.js"}); }
            catch (e) { hashKind = e.kind; }
            return {saveMsg: saveMsg, hashKind: hashKind};"#,
        );
        assert!(
            v["saveMsg"]
                .as_str()
                .unwrap_or_default()
                .contains("whitespace"),
            "JS boundary rejects before any write: {v}"
        );
        assert_eq!(v["hashKind"], json!("denied"), "host channel refuses: {v}");
        assert!(
            !dir.join(LIB_INDEX_REL).exists(),
            "no poisoned INDEX was written"
        );
        // Sparse file just over the cap — the read is bounded, the refusal
        // is the cap error (not an unbounded slurp into host memory).
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        let big = std::fs::File::create(dir.join("lib/big.js")).unwrap();
        big.set_len(MAX_CODE_BYTES as u64 + 4096).unwrap();
        drop(big);
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            try { tools.call("__hash_file", {path: "lib/big.js"}); return {capped: false}; }
            catch (e) { return {capped: true, kind: e.kind, msg: String(e)}; }"#,
        );
        assert_eq!(v["capped"], json!(true), "{}", v);
        assert_eq!(v["kind"], json!("lib"), "{}", v);
        assert!(
            v["msg"]
                .as_str()
                .unwrap_or_default()
                .contains("lib read cap"),
            "{}",
            v
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// External audit P1-2: the lib family honors Hooks::Modify — the
    /// EFFECTIVE path is re-validated against the lib boundary and
    /// deny_paths before execution (a host redirecting unsafe→safe must
    /// actually land on safe), the trace carries the dual hash, and a
    /// Modify that produces an inadmissible path refuses (kind="denied")
    /// instead of silently running the original.
    #[test]
    fn lib_modify_revalidated_dual_hashed() {
        struct Redirect {
            from: &'static str,
            to: &'static str,
        }
        impl crate::hooks::Hooks for Redirect {
            fn before_tool(
                &self,
                tool: &str,
                input: &Value,
                _c: &ToolCtx,
            ) -> crate::hooks::ToolDecision {
                if (tool == "include" || tool == "hash_file")
                    && input.get("path").and_then(Value::as_str) == Some(self.from)
                {
                    crate::hooks::ToolDecision::Modify(json!({ "path": self.to }))
                } else {
                    crate::hooks::ToolDecision::Allow
                }
            }
        }
        let dir = scratch();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/unsafe.js"), "globalThis.loaded = 'unsafe';").unwrap();
        std::fs::write(dir.join("lib/safe.js"), "globalThis.loaded = 'safe';").unwrap();
        let idx = format!(
            "<!-- ta-lib: path=unsafe.js sha256={} needs=- ver=1 st=ok -->\n\
             <!-- ta-lib: path=safe.js sha256={} needs=- ver=1 st=ok -->\n",
            sha256_hex(b"globalThis.loaded = 'unsafe';"),
            sha256_hex(b"globalThis.loaded = 'safe';"),
        );
        std::fs::write(dir.join(LIB_INDEX_REL), idx).unwrap();
        let safe_hex = sha256_hex(b"globalThis.loaded = 'safe';");
        let mut g = ctx(&dir);
        g.hooks = Some(std::sync::Arc::new(Redirect {
            from: "lib/unsafe.js",
            to: "lib/safe.js",
        }));
        // ① include: the redirect lands on the EFFECTIVE path.
        let out = lib_tool()
            .run_with_emit(
                &json!({"code": r#"
                    include("lib/unsafe.js");
                    return { loaded: globalThis.loaded };"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["loaded"], json!("safe"), "the host's redirect wins: {v}");
        // Dual-hash audit: the __include trace records the effective input.
        let calls = out.meta.as_ref().unwrap()["calls"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let trace = calls
            .iter()
            .find(|c| c["name"] == json!("__include"))
            .expect("lib trace");
        assert!(
            trace["in_hash_eff"].is_string(),
            "dual hash recorded: {trace}"
        );
        // ② hash_file family redirects too — the hash is safe.js's.
        let out = lib_tool()
            .run_with_emit(
                &json!({"code": r#"
                    var h = tools.call("__hash_file", {path: "lib/unsafe.js"});
                    return { sha: h.sha256 };"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(
            v["sha"],
            json!(safe_hex),
            "hash_file hashes the redirected file: {v}"
        );
        // ③ A Modify to a path outside lib/ refuses — the original never runs.
        let mut g2 = ctx(&dir);
        g2.hooks = Some(std::sync::Arc::new(Redirect {
            from: "lib/unsafe.js",
            to: "../evil.js",
        }));
        let out = lib_tool()
            .run_with_emit(
                &json!({"code": r#"
                    try { include("lib/unsafe.js"); return { refused: false }; }
                    catch (e) { return { refused: true, kind: e.kind, msg: String(e) }; }"#}),
                &g2,
                &mut |_| {},
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["refused"], json!(true), "{}", v);
        assert_eq!(v["kind"], json!("denied"), "{}", v);
        assert!(
            v["msg"].as_str().unwrap_or_default().contains("Modify"),
            "the refusal names the hooks rewrite: {v}"
        );
        // ④ A Modify onto a deny_paths pattern refuses the same way.
        let strict = tool_with_tools(
            CodeConfig {
                include: true,
                deny_paths: vec!["lib/safe.js".to_string()],
                ..CodeConfig::default()
            },
            r#"["read_file","write_file","list_dir"]"#,
        );
        let out = strict
            .run_with_emit(
                &json!({"code": r#"
                    try { include("lib/unsafe.js"); return { refused: false }; }
                    catch (e) { return { refused: true, kind: e.kind, msg: String(e) }; }"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["refused"], json!(true), "{}", v);
        assert_eq!(v["kind"], json!("denied"), "{}", v);
        assert!(
            v["msg"].as_str().unwrap_or_default().contains("deny_paths"),
            "the refusal names the policy layer: {v}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-3: a saveLib whose INDEX read fails (hooks Deny, IO error, paging
    /// anomaly) ABORTS — it must not treat the unreadable index as empty
    /// and overwrite every existing registration and approval. Only a
    /// definite not-found initializes a fresh index (covered by every
    /// first-saveLib test above).
    #[test]
    fn lib_save_index_read_error_preserves_old_index() {
        struct DenyIndexRead;
        impl crate::hooks::Hooks for DenyIndexRead {
            fn before_tool(
                &self,
                tool: &str,
                input: &Value,
                _c: &ToolCtx,
            ) -> crate::hooks::ToolDecision {
                if tool == "read_file"
                    && input.get("path").and_then(Value::as_str) == Some("lib/INDEX.md")
                {
                    crate::hooks::ToolDecision::Deny("index reads are audited".into())
                } else {
                    crate::hooks::ToolDecision::Allow
                }
            }
        }
        let dir = scratch();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/a.js"), "globalThis.a = 1;").unwrap();
        std::fs::write(dir.join("lib/b.js"), "globalThis.b = 2;").unwrap();
        let idx = format!(
            "<!-- ta-lib: path=a.js sha256={} needs=- ver=1 st=ok -->\n\
             <!-- ta-lib: path=b.js sha256={} needs=- ver=1 st=ok -->\n",
            sha256_hex(b"globalThis.a = 1;"),
            sha256_hex(b"globalThis.b = 2;"),
        );
        std::fs::write(dir.join(LIB_INDEX_REL), &idx).unwrap();
        let mut g = ctx(&dir);
        g.hooks = Some(std::sync::Arc::new(DenyIndexRead));
        let out = lib_tool()
            .run_with_emit(
                &json!({"code": r#"
                    var err = null;
                    try { saveLib("lib/c.js", "var c = 1;"); } catch (e) { err = String(e); }
                    var includeOldOk = false;
                    try { include("lib/a.js"); includeOldOk = true; } catch (e) {}
                    return { err: err, includeOldOk: includeOldOk };"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert!(
            v["err"]
                .as_str()
                .unwrap_or_default()
                .contains("index reads are audited"),
            "the original read error propagates (no silent overwrite): {v}"
        );
        assert_eq!(v["includeOldOk"], json!(true), "old lib still admits: {v}");
        // The INDEX is byte-identical — registrations and approvals intact.
        assert_eq!(
            std::fs::read_to_string(dir.join(LIB_INDEX_REL)).unwrap(),
            idx
        );
        // The lib-file half of the write did land (the documented partial
        // state — same shape as the INDEX-unusable leg above).
        assert!(dir.join("lib/c.js").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lib_savelib_registers_canonical_path() {
        let dir = scratch();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            saveLib("lib/./new.js", "globalThis.n = 1;");
            saveLib("lib/../lib/z.js", "globalThis.z = 1;");
            include("lib/new.js");
            include("lib/z.js");
            return { n: globalThis.n, z: globalThis.z };"#,
        );
        assert_eq!(v["n"], json!(1), "canonical spelling admits: {v}");
        assert_eq!(v["z"], json!(1), "canonical spelling admits: {v}");
        let index = std::fs::read_to_string(dir.join(LIB_INDEX_REL)).unwrap();
        assert!(index.contains("path=new.js"), "canonical key: {index}");
        assert!(index.contains("path=z.js"), "canonical key: {index}");
        assert_eq!(
            index.matches("ta-lib:").count(),
            2,
            "no duplicate/spelling-variant entries: {index}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// External re-review P1: a hooks denial whose TEXT happens to contain
    /// an OS "not found" phrasing must not spoof the empty-index
    /// initialization. The old guest-side saveLib classified by string
    /// matching (`indexOf("No such file or directory")`), so this denial
    /// wiped every existing registration; the existence probe is now a host
    /// filesystem fact (`__lib_index_state`), and an EXISTING index is read
    /// through the gated read_file — the denial aborts the save instead.
    #[test]
    fn lib_spoofed_not_found_hooks_message_preserves_index() {
        struct SpoofingDeny;
        impl crate::hooks::Hooks for SpoofingDeny {
            fn before_tool(
                &self,
                tool: &str,
                input: &Value,
                _c: &ToolCtx,
            ) -> crate::hooks::ToolDecision {
                if tool == "read_file"
                    && input.get("path").and_then(Value::as_str) == Some("lib/INDEX.md")
                {
                    // The exact probe shape from the re-review: a plausible
                    // approval-backend message embedding the ENOENT text.
                    crate::hooks::ToolDecision::Deny(
                        "approval backend unavailable: No such file or directory".into(),
                    )
                } else {
                    crate::hooks::ToolDecision::Allow
                }
            }
        }
        let dir = scratch();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/unsafe.js"), "globalThis.u = 1;").unwrap();
        std::fs::write(dir.join("lib/safe.js"), "globalThis.s = 2;").unwrap();
        let idx = format!(
            "<!-- ta-lib: path=unsafe.js sha256={} needs=- ver=1 st=ok -->\n\
             <!-- ta-lib: path=safe.js sha256={} needs=- ver=1 st=ok -->\n",
            sha256_hex(b"globalThis.u = 1;"),
            sha256_hex(b"globalThis.s = 2;"),
        );
        std::fs::write(dir.join(LIB_INDEX_REL), &idx).unwrap();
        let mut g = ctx(&dir);
        g.hooks = Some(std::sync::Arc::new(SpoofingDeny));
        let out = lib_tool()
            .run_with_emit(
                &json!({"code": r#"
                    var err = null;
                    try { saveLib("lib/spoof.js", "var sp = 1;"); } catch (e) { err = String(e); }
                    var includeOldOk = false;
                    try { include("lib/safe.js"); includeOldOk = true; } catch (e) {}
                    return { err: err, includeOldOk: includeOldOk };"#}),
                &g,
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert!(
            v["err"]
                .as_str()
                .unwrap_or_default()
                .contains("approval backend unavailable"),
            "the spoofing denial propagates (no empty-index overwrite): {v}"
        );
        assert_eq!(v["includeOldOk"], json!(true), "old lib still admits: {v}");
        // Both original registrations intact, byte-identical — this is the
        // exact wipe the probe used to cause.
        assert_eq!(
            std::fs::read_to_string(dir.join(LIB_INDEX_REL)).unwrap(),
            idx
        );
        assert!(dir.join("lib/spoof.js").exists(), "documented half state");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// External re-review P2: a pre-existing INDEX line the HOST parser
    /// rejects (short sha, non-numeric ver, st outside {ok,draft}) must
    /// abort the save — the old guest-side near-copy of the parser only
    /// checked the five keys' presence, let the poisoned line through the
    /// write-back, and every later include died on the host parse. The
    /// merge now runs the SAME parser (`parse_lib_line` via
    /// `lib_upsert_index`), so the poison is named and the index is left
    /// untouched for the still-valid entries.
    #[test]
    fn lib_malformed_index_line_rejected_at_save() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/good.js"), "globalThis.g = 1;").unwrap();
        let idx = format!(
            "<!-- ta-lib: path=bad.js sha256=xyz needs=- ver=abc st=approved -->\n\
             <!-- ta-lib: path=good.js sha256={} needs=- ver=1 st=ok -->\n",
            sha256_hex(b"globalThis.g = 1;"),
        );
        std::fs::write(dir.join(LIB_INDEX_REL), &idx).unwrap();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            var err = null;
            try { saveLib("lib/new.js", "var nw = 1;"); } catch (e) { err = String(e); }
            var includeOldOk = false;
            try { include("lib/good.js"); includeOldOk = true; } catch (e) {}
            return { err: err, includeOldOk: includeOldOk };"#,
        );
        assert!(
            v["err"]
                .as_str()
                .unwrap_or_default()
                .contains("line 1: sha256 must be 64 hex chars"),
            "the host parser names the poisoned line: {v}"
        );
        // The poisoned index legitimately blocks admission for EVERY entry
        // (parse_lib_index is strict by design) — which is exactly why the
        // save must refuse instead of writing a new registration alongside
        // the poison and reporting success.
        assert_eq!(
            v["includeOldOk"],
            json!(false),
            "a poisoned index blocks all includes (pre-existing strictness): {v}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(LIB_INDEX_REL)).unwrap(),
            idx,
            "rejected merge leaves the index untouched"
        );
        assert!(dir.join("lib/new.js").exists(), "documented half state");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// External re-review P3: `__include` delivers the source wrapped as
    /// {"src": S}. The envelope glue auto-JSON-parses content, so a library
    /// whose WHOLE text is valid JSON (a data-file lib) used to arrive as an
    /// object — `(0, eval)(object)` is a silent no-op, the include
    /// "succeeded" while loading nothing. Wrapped, the parse lands on the
    /// envelope and the source lands as the string it is; an object-shaped
    /// JSON lib then evals honestly (SyntaxError in statement position)
    /// instead of vanishing.
    #[test]
    fn lib_whole_file_json_source_arrives_as_string() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        let json_src = "{\"marker\": 7}";
        let plain_src = "globalThis.fromPlain = 'yes';";
        std::fs::write(dir.join("lib/data.json"), json_src).unwrap();
        std::fs::write(dir.join("lib/plain.js"), plain_src).unwrap();
        std::fs::write(
            dir.join(LIB_INDEX_REL),
            format!(
                "<!-- ta-lib: path=data.json sha256={} needs=- ver=1 st=ok -->\n\
                 <!-- ta-lib: path=plain.js sha256={} needs=- ver=1 st=ok -->\n",
                sha256_hex(json_src.as_bytes()),
                sha256_hex(plain_src.as_bytes()),
            ),
        )
        .unwrap();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            var direct = tools.call("__include", {path: "lib/data.json"});
            var threw = false, err = null;
            try { include("lib/data.json"); } catch (e) { threw = true; err = String(e); }
            include("lib/plain.js");
            return {
                directIsStr: typeof direct === "object" && typeof direct.src === "string",
                srcExact: direct && direct.src === '{"marker": 7}',
                threw: threw, err: err,
                plain: typeof globalThis.fromPlain };"#,
        );
        // The wire shape: source arrives as the exact string, never
        // auto-parsed into the JSON value.
        assert_eq!(v["directIsStr"], json!(true), "{}", v);
        assert_eq!(v["srcExact"], json!(true), "{}", v);
        // Statement-position object literal: an honest SyntaxError, not the
        // old silent no-op success.
        assert_eq!(v["threw"], json!(true), "{}", v);
        // Control leg: a plain JS lib through the SAME wrapped channel still
        // lands its declarations.
        assert_eq!(v["plain"], json!("string"), "{}", v);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// External re-review P3: an INDEX that exists but cannot be READ
    /// (permissions, a directory in the way) reports the IO error as what it
    /// is — the old `unwrap_or_default()` folded it into "no entry",
    /// misdirecting the guest to saveLib for an index it cannot even read.
    #[test]
    fn lib_index_read_error_is_not_no_entry() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/a.js"), "globalThis.a = 1;").unwrap();
        // A directory where INDEX.md should be: read_to_string fails with
        // EISDIR — an IO error that is definitively not NotFound.
        std::fs::create_dir_all(dir.join(LIB_INDEX_REL)).unwrap();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            var err = null;
            try { include("lib/a.js"); } catch (e) { err = String(e); }
            return { err: err };"#,
        );
        let err = v["err"].as_str().unwrap_or_default();
        assert!(
            err.contains("cannot read lib/INDEX.md"),
            "the IO error is named as an IO error: {err}"
        );
        assert!(
            !err.contains("has no entry"),
            "not misdirected to saveLib: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M2 negative legs: the include channel is an INDIRECT eval in global
    /// scope — `function`/`var` declarations leak to globalThis (the export
    /// contract), while lexical declarations (`let`/`const`/`class`) stay in
    /// the eval's own environment, and a `'use strict'` prologue confines
    /// even `function`/`var`. A QuickJS change to eval scoping must trip
    /// this pin, not a downstream task.
    #[test]
    fn lib_lexical_and_strict_declarations_not_exported() {
        let dir = scratch();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            saveLib("lib/lex.js",
                "function fn() { return 1; }\nvar vv = 2;\nlet ll = 3;\nconst cc = 4;\nclass KK {}\nglobalThis.ran = true;");
            saveLib("lib/strict.js", "'use strict';\nfunction sf() { return 9; }\nvar sv = 9;");
            include("lib/lex.js");
            include("lib/strict.js");
            return {
                fn: typeof fn, vv: typeof vv,
                ll: typeof ll, cc: typeof cc, kk: typeof KK,
                ran: globalThis.ran === true,
                sf: typeof sf, sv: typeof sv,
            };"#,
        );
        assert_eq!(v["fn"], json!("function"), "functions export: {v}");
        assert_eq!(v["vv"], json!("number"), "vars export: {v}");
        for leg in ["ll", "cc", "kk"] {
            assert_eq!(
                v[leg],
                json!("undefined"),
                "lexical declaration '{leg}' must not export: {v}"
            );
        }
        assert_eq!(v["ran"], json!(true), "side effects of the load run: {v}");
        for leg in ["sf", "sv"] {
            assert_eq!(
                v[leg],
                json!("undefined"),
                "strict-mode lib must not export '{leg}': {v}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M10's last leg — the recorded LIMITATION, pinned so it stays a
    /// documented fact rather than a regression: the include gate does not
    /// cover every execution path. A guest can always `readAll` a lib file
    /// (reads are not lib-gated) and `eval` it itself — the lib channel's
    /// admission is load-channel hygiene, not a containment boundary.
    #[test]
    fn lib_readall_eval_still_works() {
        let dir = scratch();
        // st=draft + no session registration → include must refuse…
        plant_lib(&dir, "x.js", "globalThis.viaEval = 41;", "draft");
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            var refused = null;
            try { include("lib/x.js"); refused = false; }
            catch (e) { refused = e.kind; }
            // …but readAll + eval is ordinary guest code and still runs.
            eval(readAll("lib/x.js"));
            return {refused: refused, viaEval: (globalThis.viaEval || 0) + 1};"#,
        );
        assert_eq!(v["refused"], json!("lib"), "draft admission refuses: {v}");
        assert_eq!(
            v["viaEval"],
            json!(42),
            "readAll+eval bypasses admission: {v}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M11: multi-lib upsert never crosses INDEX entries, and a failed
    /// INDEX write (here: a directory squatting on the INDEX path) errors
    /// the whole saveLib WITHOUT leaving a half-registration that admits —
    /// the lib file may exist on disk, but include still refuses.
    #[test]
    fn lib_index_upsert_and_partial_failure() {
        let dir = scratch();
        js_out(
            &lib_tool(),
            &dir,
            r#"
            saveLib("lib/a.js", "globalThis.a = 1;");
            saveLib("lib/b.js", "globalThis.b = 2;");
            saveLib("lib/a.js", "globalThis.a = 10;");
            return 1;"#,
        );
        let index = std::fs::read_to_string(dir.join(LIB_INDEX_REL)).unwrap();
        assert_eq!(
            index.matches("ta-lib:").count(),
            2,
            "upsert replaces, never duplicates: {index}"
        );
        assert!(
            index.contains("path=a.js") && index.contains("path=b.js"),
            "both entries intact: {index}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("lib/a.js")).unwrap(),
            "globalThis.a = 10;",
            "upsert wrote the new content"
        );
        // Partial-failure leg: the INDEX path is unusable → saveLib errors…
        let dir2 = scratch();
        std::fs::create_dir_all(dir2.join("lib/INDEX.md")).unwrap();
        let out = lib_tool()
            .run_with_emit(
                &json!({"code": r#"saveLib("lib/c.js", "var c = 1;"); return 1;"#}),
                &ctx(&dir2),
                &mut |_| {},
            )
            .unwrap();
        assert!(
            out.is_error,
            "INDEX write failure surfaces: {}",
            out.content
        );
        assert!(
            dir2.join("lib/c.js").exists(),
            "the lib file half of the write did land — this is the partial state"
        );
        // …and the half-state does not admit: no entry, refused.
        let v = js_out(
            &lib_tool(),
            &dir2,
            r#"
            try { include("lib/c.js"); return {admitted: true}; }
            catch (e) { return {admitted: false, kind: e.kind}; }"#,
        );
        assert_eq!(v["admitted"], json!(false), "{}", json!(v));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// A-M4 (folded into the audit assertions above) + A-M9: gate OFF is the
    /// old binary's model-visible contract — no prelude helpers, reserved-name
    /// direct calls refused naming the knob, no 【库】 in the description.
    #[test]
    fn gate_off_matches_old_contract() {
        let out = run_code(
            r#"return [typeof include, typeof saveLib];"#,
            CodeConfig::default(),
        );
        assert_eq!(out.content, "[\"undefined\",\"undefined\"]");
        let out = run_code(
            r#"
            try { tools.call("__include", {path: "lib/x.js"}); return {refused: false}; }
            catch (e) { return {refused: true, kind: e.kind, msg: String(e)}; }"#,
            CodeConfig::default(),
        );
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["refused"], json!(true), "{}", out.content);
        assert_eq!(v["kind"], json!("denied"), "{}", out.content);
        assert!(
            v["msg"]
                .as_str()
                .unwrap_or("")
                .contains("code.include=false"),
            "refusal names the knob: {}",
            out.content
        );
        // Both lib-family reserved names refuse the same way when gated off
        // (MD-2: hash is a gated host action, not the introspection class).
        let out = run_code(
            r#"
            try { tools.call("__hash_file", {path: "lib/x.js"}); return {refused: false}; }
            catch (e) { return {refused: true, kind: e.kind, msg: String(e)}; }"#,
            CodeConfig::default(),
        );
        let v: Value = serde_json::from_str(&out.content).unwrap_or(json!(null));
        assert_eq!(v["refused"], json!(true), "{}", out.content);
        assert_eq!(v["kind"], json!("denied"), "{}", out.content);
        assert!(
            v["msg"]
                .as_str()
                .unwrap_or("")
                .contains("code.include=false"),
            "hash refusal names the same knob: {}",
            out.content
        );
        let d = tool_with(CodeConfig::default()).def().description;
        assert!(
            !d.contains("【库】") && !d.contains("saveLib"),
            "gate-off description must stay the old contract"
        );
    }

    /// MD-13 gate-ON description: the 【库】 overlay names exactly the two
    /// new helpers and stays slim (bounded like the base).
    #[test]
    fn gate_on_description_lists_lib_helpers() {
        let d = tool_with(include_cfg()).def().description;
        assert!(d.contains("【库】"), "{d}");
        assert!(
            d.contains("include(path)") && d.contains("saveLib(path, src)"),
            "{d}"
        );
        // Base contract unchanged underneath the overlay.
        assert!(d.contains("readAll") && d.contains("writeVerified"), "{d}");
        assert!(
            d.chars().count() <= 1500,
            "gate-on description grew to {} chars",
            d.chars().count()
        );
    }

    /// saveLib's own input check: non-lib/ paths throw before any write.
    #[test]
    fn save_lib_requires_lib_prefix() {
        let dir = scratch();
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            try { saveLib("out/x.js", "var x = 1;"); return {threw: false}; }
            catch (e) { return {threw: true, msg: String(e)}; }"#,
        );
        assert_eq!(v["threw"], json!(true), "{v}");
        assert!(
            v["msg"]
                .as_str()
                .unwrap_or("")
                .contains("must start with lib/"),
            "{v}"
        );
        assert!(!dir.join("out").exists(), "nothing written on refusal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A-M6 lifecycle e2e: 起草登记 → 新会话 draft 拒 → 负责人批准相同 hash →
    /// 加载 → 局部覆写（本次生效、文件不变）→ saveLib 写回回 draft（原批准作废）
    /// → 再批准后才复可用；内容未变时重新登记可救活。
    #[test]
    fn lib_lifecycle_e2e() {
        let dir = scratch();
        // 1. 起草登记（会话 A）。
        let tool_a = lib_tool();
        let out = tool_a
            .run_with_emit(
                &json!({"code": r#"saveLib("lib/judge.js", "function judge(x) { return x > 0 ? 'pos' : 'neg'; }"); return 1;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // 2. 新会话（fresh lineage）直接加载 draft → 拒。
        let tool_b = lib_tool();
        let v = js_out(
            &tool_b,
            &dir,
            r#"
            try { include("lib/judge.js"); return {refused: false}; }
            catch (e) { return {refused: true}; }"#,
        );
        assert_eq!(v["refused"], json!(true), "draft needs approval: {v}");
        // 3. 负责人批准相同 hash（st: draft → ok，内容不动）。
        let index = std::fs::read_to_string(dir.join(LIB_INDEX_REL)).unwrap();
        std::fs::write(dir.join(LIB_INDEX_REL), index.replace("st=draft", "st=ok")).unwrap();
        // 4. 加载（无会话登记，纯 st=ok 准入）+ 5. 局部覆写本次生效、文件不变。
        let v = js_out(
            &tool_b,
            &dir,
            r#"
            include("lib/judge.js");
            function judge(x) { return x > 0 ? 'POS' : 'neg'; }
            return {overridden: judge(1), onDiskStillOld: readAll("lib/judge.js").indexOf("POS") === -1};"#,
        );
        assert_eq!(v["overridden"], json!("POS"), "{v}");
        assert_eq!(v["onDiskStillOld"], json!(true), "{v}");
        // 6. 写回：新哈希登记、回 draft。
        let out = tool_b
            .run_with_emit(
                &json!({"code": r#"
                    include("lib/judge.js");
                    function judge(x) { return x > 0 ? 'POSITIVE' : 'neg'; }
                    saveLib("lib/judge.js", "function judge(x) { return x > 0 ? 'POSITIVE' : 'neg'; }");
                    return 1;"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let index = std::fs::read_to_string(dir.join(LIB_INDEX_REL)).unwrap();
        assert!(
            index.contains("st=draft"),
            "write-back resets draft: {index}"
        );
        // 7. 原批准作废：fresh lineage 再次被拒（回 draft 且其会话未登记）。
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            try { include("lib/judge.js"); return {refused: false}; }
            catch (e) { return {refused: true}; }"#,
        );
        assert_eq!(
            v["refused"],
            json!(true),
            "approval died with the old hash: {v}"
        );
        // 8. 内容未变时重新登记可救活（同 hash，本会话登记）。
        let v = js_out(
            &lib_tool(),
            &dir,
            r#"
            var src = readAll("lib/judge.js");
            saveLib("lib/judge.js", src);
            include("lib/judge.js");
            return judge(2);"#,
        );
        assert_eq!(v, json!("POSITIVE"), "re-register same bytes revives: {v}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── INDEX 解析/登记性质测试（固定种子 LCG，零依赖，纯函数） ──────────────

    /// Deterministic 64-bit LCG (fixed seed): the property tests' only source
    /// of randomness. Same seed → same draws on every run and platform.
    struct Lcg(u64);
    impl Lcg {
        fn next_u64(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next_u64() % n as u64) as usize
        }
        fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
            &xs[self.below(xs.len())]
        }
    }

    fn fuzz_hex(rng: &mut Lcg) -> String {
        const HEX: &[char] = &['0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'f'];
        (0..64).map(|_| *rng.pick(HEX)).collect()
    }

    fn fuzz_rel(rng: &mut Lcg) -> String {
        // 合法登记键：1-3 段，段内 [a-z0-9_-]，无空白、无 `..`、无空段。
        const SEGS: &[&str] = &["a", "b1", "lib-x", "z_9", "tool.js", "n.json"];
        (0..1 + rng.below(3))
            .map(|_| *rng.pick(SEGS))
            .collect::<Vec<_>>()
            .join("/")
    }

    /// Property (fuzz, fixed seed): 随机合法五元组渲染成机器行 →
    /// parse_lib_line 原样解析回同样的 (path, sha256, needs, ver, st)；
    /// parse_lib_index 聚合多行不串行；lib_upsert_index 的输出再解析回
    /// 预期的登记（新条目 st=draft/needs=-/ver=1，覆盖保 needs/ver），
    /// prose 行逐字节保留，重复 upsert 幂等。
    #[test]
    fn lib_index_roundtrip_and_upsert_properties() {
        let mut rng = Lcg(0x5EED_0030);
        for _ in 0..200 {
            let (rel, sha) = (fuzz_rel(&mut rng), fuzz_hex(&mut rng));
            let needs = *rng.pick(&["-", "a", "a,b", "dep1,dep2"]);
            let ver: String = (0..1 + rng.below(3))
                .map(|_| *rng.pick(&['1', '2', '7', '9']))
                .collect();
            let st = *rng.pick(&["ok", "draft"]);

            // 1) 单行 round-trip：渲染 → 解析 → 字段逐一相等。
            let line =
                format!("<!-- ta-lib: path={rel} sha256={sha} needs={needs} ver={ver} st={st} -->");
            let parsed = parse_lib_line(1, &line)
                .expect("legal line parses")
                .unwrap();
            assert_eq!(parsed.path, rel);
            assert_eq!(parsed.sha256, sha);
            assert_eq!(parsed.needs, needs);
            assert_eq!(parsed.ver, ver);
            assert_eq!(parsed.st, st);

            // 2) 多行聚合不串行：prose + 两条登记，映射键值各自精确。
            let mut rel2 = fuzz_rel(&mut rng);
            while rel2 == rel {
                rel2 = fuzz_rel(&mut rng);
            }
            let sha2 = fuzz_hex(&mut rng);
            let line2 = format!("<!-- ta-lib: path={rel2} sha256={sha2} needs=- ver=3 st=ok -->");
            let text = format!("# index\n{line}\n自由文本,可含 | 与空格\n{line2}\n");
            let map = parse_lib_index(&text).expect("multi-entry parses");
            assert_eq!(map.len(), 2, "{text:?}");
            assert_eq!(map[&rel].sha256, sha);
            assert_eq!(map[&rel].st, st);
            assert_eq!(map[&rel2].sha256, sha2);
            assert_eq!(map[&rel2].st, "ok");

            // 3) upsert 空文本 → 追加一行，字段按 MD-10 默认（needs=- ver=1
            //    st=draft），输出再解析回同样的登记。
            let merged = lib_upsert_index(&rel, &sha, "").expect("upsert empty");
            let got = parse_lib_index(&merged).expect("merged parses");
            assert_eq!(got.len(), 1);
            assert_eq!(got[&rel].sha256, sha);
            assert_eq!(got[&rel].st, "draft");
            assert_eq!(got[&rel].needs, "-");
            assert_eq!(got[&rel].ver, "1");

            // 4) upsert 已有文本：同名条目原位替换（保 needs/ver、st 归
            //    draft），其余行（含 prose）逐字节保留。
            let new_sha = fuzz_hex(&mut rng);
            let merged2 = lib_upsert_index(&rel, &new_sha, &text).expect("upsert existing");
            for l in text.lines() {
                if l.contains(&format!("path={rel} ")) {
                    assert!(!merged2.contains(l), "old line must be replaced: {l:?}");
                } else {
                    assert!(
                        merged2.contains(l),
                        "unrelated line preserved byte-exact: {l:?}"
                    );
                }
            }
            let got2 = parse_lib_index(&merged2).expect("merged2 parses");
            assert_eq!(got2[&rel].sha256, new_sha);
            assert_eq!(got2[&rel].st, "draft");
            assert_eq!(got2[&rel].needs, needs, "needs preserved on replace");
            assert_eq!(got2[&rel].ver, ver, "ver preserved on replace");
            assert_eq!(got2[&rel2].sha256, sha2, "other entry untouched");

            // 5) 幂等：对 upsert 结果重复同一 upsert，输出不变。
            let merged3 = lib_upsert_index(&rel, &new_sha, &merged2).expect("re-upsert");
            assert_eq!(merged3, merged2);
        }
    }

    /// Property (fuzz, fixed seed): 随机劣化机器行（缺字段/重复字段/bad
    /// sha/bad st/bad ver/bad path）一律硬错误而非静默通过——INDEX 是信任
    /// 清单，畸形行绝不静默吞;随机 prose 行（不含机器行前后缀）一律
    /// Ok(None)。
    #[test]
    fn lib_index_fuzz_mutations_are_hard_errors_never_silent() {
        let mut rng = Lcg(0x5EED_0031);
        for _ in 0..200 {
            let (rel, sha) = (fuzz_rel(&mut rng), fuzz_hex(&mut rng));
            let base = format!("<!-- ta-lib: path={rel} sha256={sha} needs=- ver=1 st=draft -->");
            // 逐字段劣化,每一种都必须 Err（带行号文案）。
            let broken: Vec<String> =
                vec![
                base.replace(" st=draft", ""),                    // 缺 st
                base.replace("needs=- ", ""),                     // 缺 needs
                base.replace("ver=1", "foo=1"),                   // 未知字段
                format!("<!-- ta-lib: path={rel} path={rel} sha256={sha} needs=- ver=1 st=draft -->"), // 重复字段
                base.replace(&sha, &sha[..63]),                   // sha 短一位
                base.replace("st=draft", "st=paused"),            // 非法 st
                base.replace("ver=1", "ver="),                    // 空 ver
                base.replace(&format!("path={rel}"), "path=/abs"), // 绝对路径
                base.replace(&format!("path={rel}"), "path=x/../y"), // `..`
                base.replace(&format!("path={rel}"), "path="),    // 空 path
            ];
            for b in &broken {
                assert!(
                    parse_lib_line(1, b).is_err(),
                    "mutation must be a hard error: {b:?}"
                );
            }
            // 非 ta-lib 前缀/后缀的随机文本不是机器行 → Ok(None)，绝不 panic。
            const PROSE: &[&str] = &[
                "# my lib",
                "自由段落。",
                "<!-- 另一种注释 -->",
                "<!-- ta-lib: 未闭合",
                "ta-lib: path=x -->",
                "",
            ];
            let p = *rng.pick(PROSE);
            assert!(
                parse_lib_line(1, p)
                    .expect("prose is Ok(None), never Err")
                    .is_none(),
                "{p:?} must not parse as a machine line"
            );
        }
    }
}
