//! Guest-visible contract text: reserved-name wire constants, the prelude
//! sources, the embedded run-code skill, and the gate-ON description overlays.
//!
//! Production items are self-contained string constants + pure string
//! functions; the root import chain is needed by the tests only.

#[cfg(test)]
use super::*;

/// Sub-agent identity tag for guest-originated tool calls (design D14): hooks
/// can tell "called from run_code" apart from top-level and `task` calls. The
/// `__` prefix is reserved — `task`'s name validation rejects guest-chosen
/// names starting with it, so the marker cannot be forged by a sub-agent.
pub(super) const CODE_MARKER: &str = "__code__";

/// Hard cap on guest code size. Generous (≈ a 4k-token program); exists so a
/// runaway `code` argument fails with a clear error instead of an OOM in the
/// guest's allocator.
pub(super) const MAX_CODE_BYTES: usize = 1024 * 1024;

/// The guest-side [`ToolCtx`]: the outer call's ctx with the `__code__`
/// marker. Hooks Arc-clone along (a tool denied at the top level stays denied
/// inside the sandbox — no bypass), the budget counter and deadline are
/// shared, same as the `task` child ctx.
///
/// `max_output_chars` is deliberately NOT inherited: that knob is a
/// DISPLAY-scale budget for what enters the MODEL's context, while guest tool
/// outputs land in JS heap memory (bounded by `code.mem_mb` via the store
/// limiter) and never enter the context at all — the design's own example
/// (`SELECT … LIMIT 5000` feeding a JS aggregation) assumes data-scale
/// outputs. Real-journey A/B found the inheritance defeating the feature's
/// core value: `tools.call("read_file")` inside the sandbox returned one
/// 1000-char page no matter the requested limit, so the model fell back to 17
/// manual read_file pages through its own context and burned the run's whole
/// token budget re-reading its own history. Char unit stays (so truncate() /
/// the read_file byte-window math keep working); only the scale changes.
pub(super) const GUEST_OUTPUT_CHARS: usize = 2_000_000;

/// Built-in skill carrying run_code's full usage guide (v2 P2). The tool
/// description stays slim — identity, environment, the CLOSED helper list and
/// a pointer here — because it rides in EVERY request's tool schema; the deep
/// contracts (read_file's three modes, the byte-exact copy recipe, examples)
/// move to this skill, loaded on demand via `read_skill` as a compressible
/// tool_result instead of permanent schema. Shipped as a sibling .md file so
/// it diffs like a doc; `assemble` embeds it via `skills::embedded_entry`
/// when (and only when) the tool is enabled, and a disk skill named
/// `run-code` overrides it (the shipped guide is never a cage).
pub(crate) const RUN_CODE_SKILL_MD: &str = include_str!("../run_code_skill.md");

/// Strip one marked section: everything from the BEGIN marker through the
/// END marker (inclusive) plus the newline right after it. A malformed
/// marker set (either marker missing, or END at/before BEGIN) falls back to
/// the ORIGINAL text with a stderr warn — the markers live in the embedded
/// run_code_skill.md ASSET, so this is a data problem, not a logic error,
/// and a library crate must not panic over it: panic="abort" embedders lose
/// the whole host to a doc splice (sec-review P2). "Conservative" fallback =
/// the section stays IN (over-teaching a gated-off feature) rather than a
/// panic or a silently half-stripped body. Parameterized over `src` so the
/// malformed shapes are unit-testable against bodies other than the asset.
pub(super) fn strip_once(src: &str, begin: &str, end: &str, what: &str) -> String {
    match (src.find(begin), src.find(end)) {
        (Some(b), Some(e)) if b < e => {
            let e = e + end.len();
            let mut out = String::with_capacity(src.len());
            out.push_str(&src[..b]);
            out.push_str(src[e..].trim_start_matches('\n'));
            out
        }
        _ => {
            crate::diag::diag_warn!(
                "run_code: run_code_skill.md: {what} markers malformed — \
                 keeping the section verbatim (data problem, not a panic)"
            );
            src.to_string()
        }
    }
}

/// The skill body for one assembly (MD-13, M2): gated sections are spliced at
/// assembly time — `code.include` keeps/strips the `BEGIN/END LIB SECTION`
/// block, `code.call_batch` the `BEGIN/END BATCH SECTION` block (LD-15);
/// with every gate off the result is the pre-include/pre-batch contract.
/// Assembly-time decision (the `skill_injection` snapshot precedent): a
/// later `set_code_config` does not re-splice an already-built skill index.
/// Marker discipline: exactly one BEGIN and one END line per section, BEGIN
/// before END; a malformed marker set keeps the section verbatim with a
/// stderr warn (see [`strip_once`] — data problems never panic the crate).
pub fn run_code_skill_md(include: bool, call_batch: bool, llm: bool) -> String {
    let mut out = RUN_CODE_SKILL_MD.to_string();
    if !include {
        out = strip_once(
            &out,
            "<!-- BEGIN LIB SECTION",
            "<!-- END LIB SECTION -->",
            "LIB SECTION",
        );
    }
    if !call_batch {
        out = strip_once(
            &out,
            "<!-- BEGIN BATCH SECTION",
            "<!-- END BATCH SECTION -->",
            "BATCH SECTION",
        );
    }
    if !llm {
        out = strip_once(
            &out,
            "<!-- BEGIN LLM SECTION",
            "<!-- END LLM SECTION -->",
            "LLM SECTION",
        );
    }
    out
}

/// Guest prelude (v2 P1): a tiny, CLOSED set of I/O helpers so model-generated
/// code expresses intent instead of re-implementing paging loops and
/// line-prefix stripping — the top iteration sources in the runnability
/// sample (task1-flash ×3 was exactly a text-mode parsing contract). Three
/// helpers only: `readAll` (paged full-text read via format:"json"),
/// `readLines` (the same as an array), `writeVerified` (write then byte-exact
/// read-back compare — the no-shell byte-fidelity path). Flattened to ONE
/// physical line by [`prelude_line`] before eval: quickjs reports error line
/// numbers relative to the eval'd source, so a multi-line prelude would shift
/// every user stack frame by its length; flattened, the shift is a constant
/// +1. Discipline for additions: every new helper is a new hallucination
/// surface ("the model assumes a helper that doesn't exist") — the
/// LLM-visible description names exactly these three, keep both in lockstep.
pub(super) const PRELUDE_SRC: &str = r#"
var __TA_B64T = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
function __ta_utf8(s) {
    var u = unescape(encodeURIComponent(s)), a = new Uint8Array(u.length), i;
    for (i = 0; i < u.length; i++) a[i] = u.charCodeAt(i);
    return a;
}
function __ta_b64(a) {
    var out = [], i, n, T = __TA_B64T;
    for (i = 0; i + 2 < a.length; i += 3) {
        n = (a[i] << 16) | (a[i + 1] << 8) | a[i + 2];
        out.push(T[(n >> 18) & 63], T[(n >> 12) & 63], T[(n >> 6) & 63], T[n & 63]);
    }
    var r = a.length - i;
    if (r === 1) { n = a[i] << 16; out.push(T[(n >> 18) & 63], T[(n >> 12) & 63], "=="); }
    else if (r === 2) { n = (a[i] << 16) | (a[i + 1] << 8); out.push(T[(n >> 18) & 63], T[(n >> 12) & 63], T[(n >> 6) & 63], "="); }
    return out.join("");
}
function readAll(path, opts) {
    var out = [], off = 1, lim = (opts && opts.pageLines) || 20000;
    for (;;) {
        var r = tools.call("read_file", {path: path, offset: off, limit: lim, format: "json"});
        if (!r || typeof r !== "object" || typeof r.content !== "string") throw new Error("readAll: unexpected read_file output");
        if (r.content) out.push(r.content);
        // Pages carry their own line terminators (json content preserves
        // CRLF and the final newline), so concatenation is byte-exact — a
        // joining "\n" would inject a blank line at every page boundary.
        if (!r.truncated) return out.join("");
        if (r.next_offset == null) throw new Error("readAll: a single line exceeds the page budget at line " + off + " — page with a smaller limit or use raw reads");
        off = r.next_offset;
    }
}
function readLines(path) {
    return readAll(path).split("\n");
}
function writeVerified(path, content) {
    tools.call("write_file", {path: path, content: content});
    var bytes = __ta_utf8(content), expect = __ta_b64(bytes), off = 0;
    var CH = 1399995;
    for (;;) {
        var r = tools.call("read_file", {path: path, raw: true, offset: off, limit: CH});
        if (!r || typeof r !== "object" || typeof r.content_b64 !== "string") throw new Error("writeVerified: unexpected read_file output");
        var seg = expect.substr((off / 3) * 4, r.content_b64.length);
        if (r.content_b64 !== seg) throw new Error("writeVerified: read-back mismatch in " + path + " at byte " + off + " — the file on disk differs from what was written");
        if (!r.truncated) {
            // EOF alone is NOT success: a SHORTER file (a hooks rewrite, a
            // partial write) reads back as a matching prefix, so the slice
            // compare above passes — only the total-length check catches it.
            if (off + r.bytes_read !== bytes.length) throw new Error("writeVerified: length mismatch in " + path + " — expected " + bytes.length + " bytes on disk, read back " + (off + r.bytes_read));
            return "verified " + content.length + " chars to " + path;
        }
        off += r.bytes_read;
    }
}
tools.budget = function () { return tools.call("__budget", {}); };
"#;

/// The lib half of the prelude (code_memory_design §3.1, M1) — appended by
/// [`prelude_line`] only when `code.include` is on (MD-13: gate off = the
/// helpers do not exist, `typeof include === "undefined"`). Same closed-list
/// discipline as above: the gate-ON description names exactly these two, in
/// lockstep with the 【库】 overlay.
///
/// `include` is idempotent per path spelling and cycle-cut: the marker is
/// set BEFORE the indirect eval, so `a → b → a` loads `a` once and finishes;
/// a marker left behind on a throwing eval would poison the session, so it
/// is deleted on throw (a retry re-attempts, and fails again, honestly).
/// `(0, eval)` evaluates in global scope — the lib's declarations land on
/// the global object, visible to the calling program and to later includes.
///
/// `saveLib` = the MD-10 write path: `write_file` (full write policy
/// applies) → `__hash_file` (host hashes the delivered bytes AND registers
/// the session admission) → `__lib_index_state` (host reports whether INDEX
/// exists — a filesystem fact, so no error-string parsing can spoof the
/// empty-index initialization) → read INDEX via the ordinary gated
/// `read_file` when it exists (a hooks Deny or IO error aborts the save,
/// preserving every existing registration) → `__lib_update_index` (host-side
/// merge, the SAME parser the admission uses — st always `draft`; existing
/// `needs`/`ver` preserved, ver=1/needs=- on create) → write the merged
/// text back through the ordinary gated `write_file`. The host parses AND
/// merges; the file writes stay with the caller's ordinary file tools (see
/// [`parse_lib_line`], [`lib_upsert_index`]). The pre-existing
/// `writeVerified` (byte-fidelity write, v2 P1) is a different helper and
/// stays untouched: the design doc's "writeVerified" naming collided with
/// it, the M1 helper is `saveLib` (revision note in
/// docs/code_memory_design.md).
pub(super) const PRELUDE_LIB_SRC: &str = r#"
var __ta_libs = Object.create(null);
function include(path) {
    if (__ta_libs[path]) return undefined;
    // The host wraps the delivered source as {"src": S} — the envelope glue
    // auto-JSON-parses content, and a whole-file-JSON library would arrive
    // as an object (eval of which is a silent no-op). The wrapper takes the
    // parse; the source always lands here as the string it is.
    var src = tools.call("__include", {path: path}).src;
    __ta_libs[path] = true;
    try { (0, eval)(src); }
    catch (e) { delete __ta_libs[path]; throw e; }
    return undefined;
}
function saveLib(path, src) {
    path = String(path);
    if (path.indexOf("lib/") !== 0) throw new Error("saveLib: path must start with lib/ (got " + path + ")");
    // lib/INDEX.md machine lines are whitespace-tokenized on the host — a
    // whitespace path would register fine here and poison EVERY later
    // include with a malformed-index error. Reject before any write.
    if (/\s/.test(path)) throw new Error("saveLib: path must not contain whitespace — INDEX.md lines are whitespace-tokenized (got " + path + ")");
    tools.call("write_file", {path: path, content: src});
    var h = tools.call("__hash_file", {path: path});
    // The host returns the CANONICAL rel path it registered the content
    // under — the manifest key must be that spelling, never a raw slice of
    // the guest's input ("lib/./x.js" used to write an entry the parser
    // rejects, poisoning the whole channel).
    var rel = h && h.rel && h.rel.indexOf("lib/") === 0 ? h.rel.slice(4) : path.slice(4);
    // Existence is a HOST filesystem fact (never inferred from an error
    // string — a hooks denial whose text contains "No such file or
    // directory" must not wipe the index). When the index exists it is read
    // through the ordinary gated read_file: a hooks Deny, an IO error or a
    // paging anomaly throws here and the save ABORTS with every existing
    // registration intact.
    var state = tools.call("__lib_index_state", {});
    var text = state && state.state === "missing" ? "" : readAll("lib/INDEX.md");
    // The merge is host-side and uses the SAME parser the admission uses:
    // a pre-existing malformed or duplicated ta-lib line is rejected HERE
    // (the old guest-side near-copy of the parser had drifted and let
    // poisoned lines through). Errors throw; the index is untouched.
    var merged = tools.call("__lib_update_index", {rel: rel, sha256: h.sha256, text: text});
    tools.call("write_file", {path: "lib/INDEX.md", content: merged.index});
    return {path: rel, sha256: h.sha256, st: "draft"};
}
// The prelude's declarations live in the OUTER eval's scope — an indirectly
// eval'd lib source runs in GLOBAL scope and cannot see them (empirically:
// ReferenceError from the cycle test). Export the closed helper set onto
// globalThis so lib code gets the same environment the calling program has.
// User code may still shadow the names in its own scope; lib code binds to
// these globals (nesting, not scoping, per the design's prelude sketch).
globalThis.include = include;
globalThis.saveLib = saveLib;
globalThis.readAll = readAll;
globalThis.readLines = readLines;
globalThis.writeVerified = writeVerified;
"#;

/// The callBatch half of the prelude (code_llm_design §4/附录 A④, M2) —
/// appended by [`prelude_line`] only when `code.call_batch` is on (LD-15:
/// gate off = `typeof tools.callBatch === "undefined"`, the pre-batch
/// contract). The wrapper is THIN on purpose: the host runs every entry
/// through the exact single-call sequence; the only guest-side duty is the
/// envelope-array convention (§4) — `__ok:true` arms become parsed contents
/// (the same auto-parse-without-unwrap the single-call glue applies),
/// `__ok:false` arms become `Error` instances carrying `e.kind` / `e.tool` /
/// `e.retryable` / `e.halted` (the additive LD-9 field). `JSON.parse` can
/// never yield an Error instance, so a successful entry cannot fake a
/// failure and vice versa (the D12 collision argument).
pub(super) const PRELUDE_BATCH_SRC: &str = r#"
function __ta_parse(s) {
    try { return JSON.parse(s); } catch (e) { return s; }
}
tools.callBatch = function (entries) {
    var arr = tools.call("__call_batch", { entries: entries });
    if (typeof arr === "string") arr = __ta_parse(arr);
    if (!arr || !Array.isArray(arr)) throw new Error("callBatch: unexpected host output");
    var out = [], i, env;
    for (i = 0; i < arr.length; i++) {
        env = arr[i];
        if (env && env.__ok === true) out.push(__ta_parse(env.content));
        else {
            var e = new Error(env && env.error ? env.error : "callBatch entry failed");
            if (env) {
                if (env.kind !== undefined) e.kind = env.kind;
                if (env.tool !== undefined) e.tool = env.tool;
                if (env.retryable !== undefined) e.retryable = env.retryable;
                if (env.halted !== undefined) e.halted = env.halted;
            }
            out.push(e);
        }
    }
    return out;
};
"#;

/// The prelude flattened to a single physical line (see PRELUDE_SRC docs).
/// Blank lines and `//`-comments are dropped, the rest joined with no
/// separator — every remaining line must therefore end with `;`, `{` or `}`
/// (the invariant test pins both properties; a separator-free join of
/// statements relying on ASI would silently glue two statements together).
/// `include_libs` / `call_batch` append the gated halves' flattened source
/// (MD-13 / LD-15: the gates decide existence, per-execution via snapshot).
pub(super) fn prelude_line(include_libs: bool, call_batch: bool) -> String {
    fn flatten(src: &str) -> String {
        src.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("//"))
            .collect()
    }
    let mut out = flatten(PRELUDE_SRC);
    if include_libs {
        out.push_str(&flatten(PRELUDE_LIB_SRC));
    }
    if call_batch {
        out.push_str(&flatten(PRELUDE_BATCH_SRC));
    }
    out
}

/// The gate-ON description overlay (MD-13): appended to the base description
/// when `code.include` is set. A separate const so the extractor in
/// scripts/sync_run_code_contract.py — which pins the gate-OFF r# literal —
/// keeps matching exactly one description. LLM-visible copy is spec: this
/// text is mirrored in docs/code_memory_design.md's appendix; the two change
/// together through its revision log.
pub(super) const LIB_DESC_SUFFIX: &str = "\n\n【库】（仅 code.include 开启时；关闭时上句成立、无以下助手）include(path)：加载 lib/ 下已登记的代码库到全局作用域（幂等，循环依赖自动截断，失败 throw）；saveLib(path, src)：写库文件并在 lib/INDEX.md 登记（恒 st=draft，本会话即可 include；跨会话复用需人工审校晋升为 ok）。库校验失败（未登记/内容与登记不符/超限）e.kind=\"lib\"，重试无意义。建库/用库/修库生命周期详见 run-code 技能的「库与复用」段。";

/// The call_batch gate-ON description overlay (LD-15/附录 A④): appended when
/// `code.call_batch` is set, outside the pinned r# literal (same extractor
/// discipline as [`LIB_DESC_SUFFIX`]). Copy is A④ finalized at landing — the
/// draft's "read_file / llm" lists `llm`, which does not exist in the
/// registry until M3; naming it now would break the closed-list
/// anti-hallucination contract, so v1 says "read_file 等" (M3 revisits,
/// through this file's doc + the design revision log).
pub(super) const BATCH_DESC_SUFFIX: &str = "\n\n【批量】（仅 code.call_batch 开启时；关闭时无此助手）tools.callBatch([{name, args}, …])：批量调用独立只读工具（read_file 等 batchable 工具；按提交序返回数组；无原子性、无回滚、部分执行是正常结果——逐条检查）。单项失败不抛、该条目为 Error 实例带 e.kind/e.tool——用 instanceof 判别，不要 JSON.stringify 失败条目（Error 序列化成 {}，细节丢失）。支持并行的工具最多 8/波，当前 read_file/MCP 仍顺序执行；依赖上一页游标的分页须顺序循环。副作用调用不在批内——写顺序 for 循环。详见 run-code 技能的「批量调用」段。";

/// The llm gate-ON description overlay (code_llm_design 附录 A，LD-15 同款
/// gate 三同源纪律：描述行 / prelude 说明 / skill 段随 gate 整体出现/消失)。
/// Appended when `code.llm.enabled` is set, outside every pinned literal.
/// The tool's OWN entry in `tools.list()` carries appendix A① verbatim
/// (see `llm.rs`); this overlay only adds the routing pointer to the
/// run_code surface.
pub(super) const LLM_DESC_SUFFIX: &str = "\n\n【LLM 判别】（仅 code.llm.enabled 开启时；关闭时无此工具）tools.call(\"llm\", {prompt, system?, model?, max_tokens?, labels?})：单次模型调用（无工具、无历史、无流式）——同构的逐项语义判断（分类/抽取/校对/打标）可用代码循环 + llm 承接，按任务质量与总成本选择；model 须在 code.llm.models 内（缺省 default_model）。声明 labels（封闭词表，≤16 项）时宿主校验输出并返回规范 label，未命中 e.kind=\"llm_output\"；参数/provider/响应异常终止（含 max_tokens 截断）为 e.kind=\"llm\"，均无 retryable，读 message 自判。词表要求必须同时写进 prompt（宿主不注入）。返回 {text, label?, usage, model}。token 成本发前预占、也计调用额度；预占拒绝为 e.kind=\"budget\"，与调用次数耗尽的恢复方式不同。判别纪律/成本纪律/级联详见 run-code 技能的「LLM 判别」段。";

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── v2 P1: prelude, structured guest I/O, wasmtime cache dir ──────────

    #[test]
    fn builtin_skill_md_is_valid_and_named_run_code() {
        // The built-in skill is embedded via include_str! with no compile-time
        // validation — pin its contract here: frontmatter parses under the
        // handwritten 6-field parser, the name matches what the slim
        // description points at, the description is within the 1024-char
        // standard cap, and the body actually carries the deep contracts the
        // description delegates to it.
        let e =
            crate::skills::embedded_entry(RUN_CODE_SKILL_MD).expect("built-in skill md must parse");
        assert_eq!(e.name, "run-code");
        assert!(e.description.chars().count() <= 1024);
        let body = RUN_CODE_SKILL_MD;
        for needle in [
            "format:\"json\"",
            "raw:true",
            "content_b64",
            "readAll",
            "readLines",
            "writeVerified",
            "next_offset",
            "BigInt",
            "1103515245n",
        ] {
            assert!(body.contains(needle), "skill body missing {needle:?}");
        }
    }

    #[test]
    fn slim_description_points_at_the_skill_and_keeps_helper_list_closed() {
        // The description is the PERMANENT schema surface: it must name the
        // closed helper list (anti-hallucination — the skill body is only
        // loaded on demand) and point at the run-code skill; and it must stay
        // slim (that's the whole point of P2) — bound it so a regression to
        // the old full-contract description fails here.
        let cc: CodeConfig = serde_json::from_str("{}").unwrap();
        let d = tool_with(cc).def().description;
        assert!(d.contains("readAll") && d.contains("readLines") && d.contains("writeVerified"));
        assert!(d.contains("此外没有别的内置助手"));
        assert!(d.contains("run-code"));
        // 数值精度契约必须常驻 schema（bench 实证：Python LCG 直译 JS 没用
        // BigInt，float64 丢低位答 816 而非 1048——描述零精度字样时模型自我背书）
        assert!(d.contains("BigInt") && d.contains("2^53"));
        assert!(
            d.chars().count() <= 1200,
            "description grew to {} chars — move the detail into run_code_skill.md",
            d.chars().count()
        );
    }

    #[test]
    fn prelude_flattens_to_exactly_one_physical_line() {
        // quickjs reports error line numbers relative to the eval'd source:
        // the flattened prelude shifts user frames by a CONSTANT +1. A
        // newline in the flattened form would break that invariant, and a
        // line not ending in `;`/`{`/`}` would glue statements together.
        // Both gate states must hold the invariant (each gated half flattens
        // under the same rules).
        for (include_libs, call_batch, src) in [
            (false, false, PRELUDE_SRC),
            (true, false, PRELUDE_LIB_SRC),
            (false, true, PRELUDE_BATCH_SRC),
        ] {
            let flat = prelude_line(include_libs, call_batch);
            assert!(!flat.contains('\n'), "flattened prelude must be one line");
            assert!(flat.starts_with("var __TA_B64T"), "unexpected head: {flat}");
            for line in src.lines().map(str::trim) {
                if line.is_empty() || line.starts_with("//") {
                    continue;
                }
                assert!(
                    line.ends_with(';') || line.ends_with('{') || line.ends_with('}'),
                    "prelude line must end in ;/{{/}} for separator-free flattening: {line:?}"
                );
            }
        }
    }

    /// readAll/readLines: paged assembly via format:"json" + next_offset,
    /// no line-number prefixes, correct line count across page boundaries.
    #[test]
    fn prelude_read_all_and_read_lines_page_through() {
        let dir = scratch();
        // 30_000 short rows → 2 pages at the default pageLines=20_000.
        let content: String = (1..=30_000).map(|i| format!("row-{i}\n")).collect();
        std::fs::write(dir.join("big.txt"), content).unwrap();
        let out = tool_with_tools(CodeConfig::default(), r#"["read_file","write_file"]"#)
            .run_with_emit(
                &json!({"code": r#"
                    var ls = readLines("big.txt");
                    var text = readAll("big.txt");
                    return {n: ls.length, first: ls[0], last: ls[ls.length - 1],
                            chars: text.length, tail: text.slice(-7)};"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // The file is 30_000 newline-TERMINATED rows (288_894 chars incl. the
        // final newline — readAll is byte-faithful), so readLines' split yields
        // a trailing empty element (n=30001, last="") — the documented
        // readLines semantics (skill md: 末尾换行会产生空尾元素).
        assert_eq!(
            out.content,
            r#"{"n":30001,"first":"row-1","last":"","chars":288894,"tail":"-30000\n"}"#
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// writeVerified: the byte-faithful write path — exact bytes on disk
    /// including a trailing newline, verified by read-back inside the guest.
    #[test]
    fn prelude_write_verified_roundtrips_exact_bytes() {
        let dir = scratch();
        let out = tool_with_tools(CodeConfig::default(), r#"["read_file","write_file"]"#)
            .run_with_emit(
                &json!({"code": r#"
                    var a = writeVerified("a.txt", "tail-no-newline");
                    var b = writeVerified("b.txt", "line1\nline2\n");
                    return [a, b];"#}),
                &ctx(&dir),
                &mut |_| {},
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read(dir.join("a.txt")).unwrap(),
            b"tail-no-newline"
        );
        assert_eq!(std::fs::read(dir.join("b.txt")).unwrap(), b"line1\nline2\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A Hooks layer that truncates every write_file body to 3 bytes — the
    /// external-review P2-2 repro shape. writeVerified's read-back used to
    /// prefix-match the shortened file (read the whole file → not truncated →
    /// the expected-prefix slice compares equal) and return
    /// "verified 6 chars" with is_error=false; only a total-length check at
    /// EOF catches a SHORTER-than-written file.
    #[test]
    fn prelude_write_verified_rejects_hook_shortened_write() {
        use crate::hooks::{Hooks, ToolDecision};

        struct TruncateTo3;
        impl Hooks for TruncateTo3 {
            fn before_tool(
                &self,
                tool: &str,
                input: &Value,
                _ctx: &crate::tools::ToolCtx,
            ) -> ToolDecision {
                if tool == "write_file" {
                    if let Some(c) = input.get("content").and_then(|v| v.as_str()) {
                        if c.len() > 3 {
                            let mut v = input.clone();
                            v["content"] = Value::String(c[..3].to_string());
                            return ToolDecision::Modify(v);
                        }
                    }
                }
                ToolDecision::Allow
            }
        }

        let dir = scratch();
        let mut guest_ctx = ctx(&dir);
        guest_ctx.hooks = Some(std::sync::Arc::new(TruncateTo3));
        let out = tool_with_tools(CodeConfig::default(), r#"["read_file","write_file"]"#)
            .run_with_emit(
                &json!({"code": r#"return writeVerified("a.txt", "abcdef");"#}),
                &guest_ctx,
                &mut |_| {},
            )
            .unwrap();
        // The hook DID rewrite the file to the 3-byte prefix; the helper must
        // refuse to call that verified instead of blessing the prefix.
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"abc");
        assert!(
            out.is_error,
            "shortened write must not verify: {}",
            out.content
        );
        assert!(
            out.content.contains("length mismatch"),
            "expected the length check to fire, got: {}",
            out.content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P2-6: saveLib registers under the host's CANONICAL path —
    /// "lib/./new.js" and "lib/../lib/z.js" both land clean `path=` keys
    /// the parser accepts, the same-session include under the plain
    /// spelling works, and no raw spelling leaks into the manifest.
    /// The design docs' frozen copy blocks must be VERBATIM the code
    /// constants (external review round 3, biggest gate blind spot: the two
    /// design docs are not in the sync manifest's hashed paths, so the A①/A④
    /// freeze blocks had drifted — a missing tail sentence, and A④ froze the
    /// RUNTIME-replaced spelling instead of the constant). A hash only
    /// proves "changed"; this proves "identical to the constant".
    #[test]
    fn frozen_doc_blocks_match_desc_constants_verbatim() {
        fn first_fenced_block_after(doc: &str, anchor: &str) -> String {
            let i = doc
                .find(anchor)
                .unwrap_or_else(|| panic!("anchor {anchor:?} missing"));
            let f0 = doc[i..].find("```").expect("no fence after anchor") + i;
            let b = doc[f0..].find('\n').expect("fence not closed on same line") + f0 + 1;
            let e = doc[b..].find("```").expect("unterminated fence") + b;
            doc[b..e].trim_end_matches('\n').to_string()
        }
        // Windows checkouts may carry CRLF (core.autocrlf); fold to LF so the
        // verbatim comparison is against the doc's content, not its on-disk
        // EOL form (same accommodation as the run_code_skill split tests).
        // The repo-root docs dir: cargo runs test binaries with CWD = the
        // package root (crates/core after the kernel split), so resolve via
        // the manifest dir instead of a bare relative path.
        let repo_docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
        let lib_doc = std::fs::read_to_string(repo_docs.join("code_memory_design.md"))
            .expect("docs/code_memory_design.md via CARGO_MANIFEST_DIR")
            .replace("\r\n", "\n");
        let llm_doc = std::fs::read_to_string(repo_docs.join("code_llm_design.md"))
            .expect("docs/code_llm_design.md via CARGO_MANIFEST_DIR")
            .replace("\r\n", "\n");
        let cases: [(&str, String, &str); 3] = [
            (
                "A① 【库】段",
                first_fenced_block_after(&lib_doc, "**① 描述（run_code 工具描述）的【库】段**"),
                LIB_DESC_SUFFIX,
            ),
            (
                "A④ callBatch 描述行",
                first_fenced_block_after(&llm_doc, "**④ callBatch 的 prelude/描述行**"),
                BATCH_DESC_SUFFIX,
            ),
            (
                "A⑤ 【LLM 判别】overlay 段",
                first_fenced_block_after(&llm_doc, "**⑤ run_code 描述的【LLM 判别】overlay 段**"),
                LLM_DESC_SUFFIX,
            ),
        ];
        for (name, block, constant) in cases {
            assert_eq!(
                block,
                constant.trim_start_matches("\n\n"),
                "{name}: the doc's frozen block must be the constant verbatim \
                 (fix the DOC to match the constant — the constant is the \
                 shipped copy)"
            );
        }
    }

    /// MD-13/M2 + LD-15/M2 + LD-14/M3: the skill's gated sections ride their
    /// assembly-time gates — off strips the marked block (gate-off skill =
    /// pre-gate contract, still frontmatter-valid), on keeps the file
    /// verbatim. Every section strips independently.
    #[test]
    fn skill_gated_sections_ride_their_gates() {
        let on = run_code_skill_md(true, true, true);
        assert!(
            on.contains("库与复用") && on.contains("批量调用") && on.contains("LLM 判别"),
            "all gates on keeps every section: {on}"
        );
        assert_eq!(on, RUN_CODE_SKILL_MD, "all gates on is the file verbatim");
        let off = run_code_skill_md(false, false, false);
        assert!(
            !off.contains("库与复用")
                && !off.contains("批量调用（需配置开启")
                && !off.contains("LLM 判别（需配置开启"),
            "gates off strip all sections: {off}"
        );
        assert!(
            !off.contains("saveLib")
                && !off.contains("BEGIN LIB SECTION")
                && !off.contains("callBatch")
                && !off.contains("BEGIN BATCH SECTION")
                && !off.contains("tools.call(\"llm\"")
                && !off.contains("BEGIN LLM SECTION"),
            "gate-off body teaches neither feature: {off}"
        );
        // The stripped skill must still parse as a valid built-in.
        assert!(
            crate::skills::embedded_entry(&off).is_some(),
            "stripped skill stays valid"
        );
        // And the gate-off body carries the rest of the guide unchanged —
        // including the shared e.kind line in its pre-gate wording (no "lib"
        // class outside the markers: the gate-off contract stays the old
        // binary's).
        assert!(off.contains("read_file 的三种模式") && off.contains("批量操作纪律"));
        assert!(
            off.contains("`tool`=工具自身失败）、`e.tool`"),
            "shared kind line keeps its pre-gate wording"
        );
        // Each gate strips only its own section.
        let lib_only = run_code_skill_md(true, false, false);
        assert!(lib_only.contains("库与复用") && !lib_only.contains("BATCH SECTION"));
        let batch_only = run_code_skill_md(false, true, false);
        assert!(!batch_only.contains("库与复用") && batch_only.contains("callBatch"));
        let llm_only = run_code_skill_md(false, false, true);
        assert!(
            !llm_only.contains("库与复用") && !llm_only.contains("BEGIN BATCH SECTION"),
            "llm gate strips neither sibling section"
        );
        assert!(
            !llm_only.contains("tools.callBatch"),
            "llm section must not teach the batch API when its gate is off — \
             pointer wording only (independent-gate discipline)"
        );
        assert!(
            llm_only.contains("LLM 判别") && llm_only.contains("级联判别"),
            "llm gate on keeps its section: {llm_only}"
        );
    }

    /// Sec-review P2: a malformed marker set is a DATA problem in the
    /// embedded asset — strip_once must fall back to the ORIGINAL text with
    /// a warn, never panic, because this crate links into panic="abort"
    /// embedders where the assembly-time splice would take the whole host
    /// down. strip_once takes the body as a parameter (hoisted out of
    /// run_code_skill_md precisely for this), so every malformed shape is
    /// testable without touching the shipped asset.
    #[test]
    fn skill_strip_once_malformed_markers_fall_back_to_the_original_text() {
        let (begin, end) = ("<!-- BEGIN X", "<!-- END X -->");
        // Well-formed reference: the block plus its trailing newline go.
        assert_eq!(
            strip_once(
                "head\n<!-- BEGIN X\nbody\n<!-- END X -->\ntail",
                begin,
                end,
                "X"
            ),
            "head\ntail"
        );
        // END without BEGIN.
        assert_eq!(
            strip_once("head\n<!-- END X -->\ntail", begin, end, "X"),
            "head\n<!-- END X -->\ntail",
            "missing BEGIN keeps the original text"
        );
        // BEGIN without END.
        assert_eq!(
            strip_once("head\n<!-- BEGIN X\nbody", begin, end, "X"),
            "head\n<!-- BEGIN X\nbody",
            "missing END keeps the original text"
        );
        // END at/before BEGIN — same fallback arm, pinned separately so a
        // future `b <= e`-style "fix" cannot quietly half-strip.
        assert_eq!(
            strip_once("<!-- END X --><!-- BEGIN X -->", begin, end, "X"),
            "<!-- END X --><!-- BEGIN X -->",
            "inverted markers keep the original text"
        );

        // The fallback warn rides the diag channel (diag.rs), not a bare
        // stderr write: a GUI host that installed a sink must receive the
        // (Warn, message) pair as data instead of losing it to a console it
        // doesn't have. Mirrors diag.rs's own sink test: serialize on
        // TEST_LOCK (the sink is process-global), assert CONTAINS rather
        // than index 0 — other tests not on the lock may emit concurrently.
        let _g = crate::diag::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let got: std::sync::Arc<std::sync::Mutex<Vec<(crate::diag::DiagLevel, String)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let g = got.clone();
        crate::diag::install_diag_sink(Box::new(move |lvl, msg| {
            g.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((lvl, msg.to_owned()))
        }));
        // Trigger one malformed shape with the sink armed.
        let kept = strip_once("head\n<!-- END X -->\ntail", begin, end, "X");
        // Clear BEFORE any assert (ordering guarantee): even a panicking
        // assertion below cannot leak the sink into the other tests.
        crate::diag::clear_diag_sink();
        assert_eq!(kept, "head\n<!-- END X -->\ntail");
        let got = got.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            got.contains(&(
                crate::diag::DiagLevel::Warn,
                "run_code: run_code_skill.md: X markers malformed — \
                 keeping the section verbatim (data problem, not a panic)"
                    .to_owned()
            )),
            "the malformed-marker warn must reach an installed sink: {got:?}"
        );
    }
}
