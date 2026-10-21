//! FR-2 static precheck (finetune_support §4): the JS lexer and the
//! fixed-order rejection copy (appendix A is the spec).
//!
//! The lexer/precheck machinery is self-contained; the root import chain is
//! needed by the tests only.

#[cfg(test)]
use super::*;

// ── FR-2 precheck (finetune_support 设计 §4) ────────────────────────────────
//
// 静态预检是效率/体验特性，不是安全机制（§4.3 诚实声明）：词法扫描可被混淆
// 绕过（globalThis["Date"]["now"]() 放行），漏检的代价仅是退回运行时失败，
// 不少任何安全保证。检出集合（FD-9 修正后）与拒答文案（附录 A，即规格）的
// 变更必须先走设计稿修订记录、再改码。

/// Lexer token — just enough for the three detection rules (§4.2). NOT a JS
/// parser: strings/templates/comments are skipped, regex literals get the
/// standard expression-position heuristic so `/Date\.now/` in a regex body
/// cannot produce a false member-expression hit.
#[derive(Debug)]
pub(super) enum Tok {
    /// Identifier or keyword text and its 1-based start line.
    Ident(String, usize),
    /// String literal content with quotes stripped but escapes NOT decoded,
    /// and its start line. The reserved-name comparison only matches plain
    /// ASCII names; an escaped spelling decodes to a reserved name only at
    /// runtime, where the reserved-name intercept still rejects it (safe
    /// direction).
    Str(String, usize),
    /// One ASCII punctuation char (no line: detection patterns never report a
    /// punct's own line — the leading ident or string literal carries it).
    Punct(char),
}

/// Keywords after which a `/` opens a regex literal, not division (an
/// expression position follows them). Not the full ECMA list — only the ones
/// that can directly precede an expression.
pub(super) const EXPR_KEYWORDS: &[&str] = &[
    "return",
    "typeof",
    "case",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "else",
    "do",
    "yield",
    "await",
    "instanceof",
];

pub(super) struct Lexer<'a> {
    b: &'a [u8],
    i: usize,
    line: usize,
    toks: Vec<Tok>,
    /// What the previous significant token implies for `/`: value-like
    /// (ident, string, number, `)`/`]`/`}`, template, regex) → division;
    /// anything else → regex start.
    prev_value: bool,
}

impl Lexer<'_> {
    /// Scan until EOF, or — when `in_expr` — until the `}` that closes a
    /// template `${…}` (consumed; the caller resumes template-literal mode).
    fn run(&mut self, in_expr: bool) {
        let mut depth = 0usize;
        while self.i < self.b.len() {
            let c = self.b[self.i];
            match c {
                b'\n' => {
                    self.line += 1;
                    self.i += 1;
                }
                b' ' | b'\t' | b'\r' => self.i += 1,
                b'/' if self.b.get(self.i + 1) == Some(&b'/') => {
                    while self.i < self.b.len() && self.b[self.i] != b'\n' {
                        self.i += 1;
                    }
                }
                b'/' if self.b.get(self.i + 1) == Some(&b'*') => {
                    self.i += 2;
                    while self.i + 1 < self.b.len()
                        && !(self.b[self.i] == b'*' && self.b[self.i + 1] == b'/')
                    {
                        if self.b[self.i] == b'\n' {
                            self.line += 1;
                        }
                        self.i += 1;
                    }
                    self.i = (self.i + 2).min(self.b.len());
                }
                b'/' if !self.prev_value => self.regex(),
                b'\'' | b'"' => self.string(c),
                b'`' => {
                    self.i += 1;
                    self.template();
                    self.prev_value = true;
                }
                b'{' => {
                    depth += 1;
                    self.punct('{');
                }
                b'}' if in_expr && depth == 0 => {
                    self.i += 1;
                    return;
                }
                b'}' => {
                    depth = depth.saturating_sub(1);
                    self.punct('}');
                }
                _ if c == b'_' || c == b'$' || c.is_ascii_alphabetic() => self.ident(),
                _ if c.is_ascii_digit() => {
                    while self.i < self.b.len()
                        && (self.b[self.i].is_ascii_alphanumeric()
                            || matches!(self.b[self.i], b'.' | b'_'))
                    {
                        self.i += 1;
                    }
                    self.prev_value = true;
                }
                _ if c.is_ascii() => {
                    let ch = c as char;
                    self.punct(ch);
                }
                _ => self.i += 1, // UTF-8 continuation byte outside a string: skip
            }
        }
    }

    fn punct(&mut self, ch: char) {
        self.toks.push(Tok::Punct(ch));
        self.prev_value = matches!(ch, ')' | ']' | '}');
        self.i += 1;
    }

    fn ident(&mut self) {
        let start = self.i;
        while self.i < self.b.len() {
            let d = self.b[self.i];
            if d == b'_' || d == b'$' || d.is_ascii_alphanumeric() {
                self.i += 1;
            } else {
                break;
            }
        }
        let text = String::from_utf8_lossy(&self.b[start..self.i]).into_owned();
        self.prev_value = !EXPR_KEYWORDS.contains(&text.as_str());
        self.toks.push(Tok::Ident(text, self.line));
    }

    fn string(&mut self, quote: u8) {
        let start_line = self.line;
        self.i += 1;
        let content_start = self.i;
        while self.i < self.b.len() && self.b[self.i] != quote {
            match self.b[self.i] {
                b'\\' if self.i + 1 < self.b.len() => self.i += 2,
                b'\n' => {
                    // A raw newline in a '…'/"…" string is invalid JS; count
                    // the line and keep scanning (recovery, not parsing).
                    self.line += 1;
                    self.i += 1;
                }
                _ => self.i += 1,
            }
        }
        let content = String::from_utf8_lossy(&self.b[content_start..self.i]).into_owned();
        if self.i < self.b.len() {
            self.i += 1; // closing quote
        }
        self.toks.push(Tok::Str(content, start_line));
        self.prev_value = true;
    }

    /// Regex literal body + flags (entry: `self.i` is ON the opening `/`).
    /// Character classes are tracked so `/[/]/` terminates correctly; a raw
    /// newline ends the scan (recovery — regexes can't span lines).
    fn regex(&mut self) {
        self.i += 1;
        let mut in_class = false;
        while self.i < self.b.len() {
            match self.b[self.i] {
                b'\\' => self.i = (self.i + 2).min(self.b.len()),
                b'[' => {
                    in_class = true;
                    self.i += 1;
                }
                b']' => {
                    in_class = false;
                    self.i += 1;
                }
                b'/' if !in_class => {
                    self.i += 1;
                    break;
                }
                b'\n' => break,
                _ => self.i += 1,
            }
        }
        while self.i < self.b.len() && self.b[self.i].is_ascii_alphabetic() {
            self.i += 1; // flags
        }
        self.prev_value = true;
    }

    /// Template literal (entry: `self.i` is just past the opening backtick).
    /// Literal parts are skipped; `${…}` expressions are lexed normally via
    /// recursive `run(true)`, so hits inside them get their real line numbers.
    fn template(&mut self) {
        while self.i < self.b.len() {
            match self.b[self.i] {
                b'\\' => self.i = (self.i + 2).min(self.b.len()),
                b'`' => {
                    self.i += 1;
                    return;
                }
                b'$' if self.b.get(self.i + 1) == Some(&b'{') => {
                    self.i += 2;
                    self.prev_value = false; // expression position starts fresh
                    self.run(true);
                }
                b'\n' => {
                    self.line += 1;
                    self.i += 1;
                }
                _ => self.i += 1,
            }
        }
    }
}

pub(super) fn lex(code: &str) -> Vec<Tok> {
    let mut lx = Lexer {
        b: code.as_bytes(),
        i: 0,
        line: 1,
        toks: Vec::new(),
        prev_value: false,
    };
    lx.run(false);
    lx.toks
}

/// Per-class hit cap (FD-11): beyond it the copy says `等 N 处` with N = the
/// class's TOTAL hit count, so one rejection still reports the true scale.
pub(super) const PRECHECK_HIT_CAP: usize = 20;

/// The FR-2 static gate (§4.2): a single lexer pass plus a fixed-order render
/// of every hit (FD-11). Returns the appendix-A rejection copy, or `None` when
/// the code is clean. Pure — unit tests call it directly and it touches
/// nothing outside its input (NFR-4).
///
/// Detection sets (FD-9 修正后，需求方数据侧同款约束）:
/// - env syntax: keywords `async`/`await`/`import` (no exemption); shadowable
///   globals `Promise`/`setTimeout`/`fetch` only when used as a call /
///   construct / member target (`X(`、`new X`、`X.`) AND not declared in-file.
/// - determinism: member expressions `Date.now` / `Math.random` (no exemption
///   — shadowing those names is pathological, and a miss only degrades to the
///   runtime stub, the safe direction).
/// - reserved tools: `tools.call("…")` with a string-literal first arg in
///   {`run_code`, `task`, `todo`}. `__tools_list`/`__budget` are deliberately
///   NOT in the set: they are WORKING read-only introspection intercepts (the
///   prelude's own `tools.budget()` routes through `__budget`) — rejecting
///   them would make the copy lie AND false-positive on legit code. A
///   non-literal first arg is not detected either; it falls through to the
///   runtime reserved-name intercept (safe direction again).
pub(super) fn precheck(code: &str) -> Option<String> {
    let toks = lex(code);
    // Declaration → exemption (FD-10): the name right after a decl keyword is
    // the model's own. The set covers the WHOLE file (hoisting order ignored),
    // then the hit pass filters by it. Known imprecision, accepted by design:
    // multi-declarator (`let a, fetch`) and destructuring don't register.
    let mut declared = std::collections::HashSet::new();
    for w in toks.windows(2) {
        if let [Tok::Ident(kw, _), Tok::Ident(name, _)] = w {
            if matches!(kw.as_str(), "function" | "const" | "let" | "var" | "class") {
                declared.insert(name.as_str());
            }
        }
    }

    let mut env: Vec<(String, usize)> = Vec::new();
    let mut det: Vec<(String, usize)> = Vec::new();
    let mut reserved: Vec<(String, usize)> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        let Tok::Ident(name, line) = t else { continue };
        // 环境语法：keywords fire unconditionally.
        if matches!(name.as_str(), "async" | "await" | "import") {
            env.push((name.clone(), *line));
            continue;
        }
        // 确定性：Date.now / Math.random member expressions.
        if matches!(name.as_str(), "Date" | "Math") {
            if let (Some(Tok::Punct('.')), Some(Tok::Ident(member, _))) =
                (toks.get(i + 1), toks.get(i + 2))
            {
                if (name == "Date" && member == "now") || (name == "Math" && member == "random") {
                    det.push((format!("{name}.{member}"), *line));
                    continue;
                }
            }
        }
        // 保留工具：tools.call("<reserved>") — the reported line is the string
        // literal's own (points at the reserved name itself).
        if name == "tools" {
            if let (
                Some(Tok::Punct('.')),
                Some(Tok::Ident(method, _)),
                Some(Tok::Punct('(')),
                Some(Tok::Str(arg, arg_line)),
            ) = (
                toks.get(i + 1),
                toks.get(i + 2),
                toks.get(i + 3),
                toks.get(i + 4),
            ) {
                if method == "call" && matches!(arg.as_str(), "run_code" | "task" | "todo") {
                    reserved.push((arg.clone(), *arg_line));
                    continue;
                }
            }
        }
        // 可遮蔽全局名：call/construct/member target AND not declared in-file.
        if matches!(name.as_str(), "Promise" | "setTimeout" | "fetch")
            && !declared.contains(name.as_str())
        {
            let as_target = matches!(toks.get(i + 1), Some(Tok::Punct('(' | '.')))
                || i.checked_sub(1)
                    .and_then(|p| toks.get(p))
                    .is_some_and(|p| matches!(p, Tok::Ident(kw, _) if kw == "new"));
            if as_target {
                env.push((name.clone(), *line));
            }
        }
    }
    render_precheck(&env, &det, &reserved)
}

pub(super) fn fmt_precheck_hits(hits: &[(String, usize)]) -> String {
    let mut s = hits
        .iter()
        .take(PRECHECK_HIT_CAP)
        .map(|(t, l)| format!("\"{t}\"（第 {l} 行）"))
        .collect::<Vec<_>>()
        .join("、");
    if hits.len() > PRECHECK_HIT_CAP {
        s.push_str(&format!("、等 {} 处", hits.len()));
    }
    s
}

/// 附录 A 即规格：以下文案逐字照抄设计稿附录 A-1/A-2/A-3，变更须先走设计稿
/// 修订记录。段序固定：环境语法 → 确定性 → 保留工具（FD-11）。
pub(super) fn render_precheck(
    env: &[(String, usize)],
    det: &[(String, usize)],
    reserved: &[(String, usize)],
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if !env.is_empty() {
        parts.push(format!(
            "run_code precheck: 检出环境不支持的语法 {}——本沙箱是同步环境：没有 async/await/Promise，没有 setTimeout 等 timer，没有 import/fetch。请改同步写法：顺序执行；批量数据在一次执行内用循环 + tools.call 逐次处理。代码未执行，无副作用；修正后重发完整代码。",
            fmt_precheck_hits(env)
        ));
    }
    if !det.is_empty() {
        parts.push(format!(
            "run_code precheck: 检出确定性违规 {}——确定性沙箱不提供 Date.now / Math.random（调用即在运行时抛错，不是返回固定值的桩）。需要时间或随机值时，改用数据记录自带的字段或代码内常量。代码未执行，无副作用；修正后重发完整代码。",
            fmt_precheck_hits(det)
        ));
    }
    // A-3 逐处一段（文案含 {name}/{line} 模板位，无法合并列举）。
    for (name, line) in reserved.iter().take(PRECHECK_HIT_CAP) {
        parts.push(format!(
            "run_code precheck: 检出 tools.call(\"{name}\")（第 {line} 行）——\"{name}\" 在沙箱内不可达（设计 D11：防自递归与嵌套成本）。沙箱内只能调用 tools.list() 列出的工具；\"{name}\" 的调用请移出沙箱、由主循环直接发起。代码未执行，无副作用；修正后重发完整代码。"
        ));
    }
    if reserved.len() > PRECHECK_HIT_CAP {
        parts.push(format!(
            "run_code precheck: 保留工具命中共 {} 处，以上仅列前 {PRECHECK_HIT_CAP} 处。代码未执行，无副作用；修正后重发完整代码。",
            reserved.len()
        ));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── FR-2 precheck (finetune_support A-3/A-4/A-5) ───────────────────────

    fn precheck_cfg() -> CodeConfig {
        CodeConfig {
            precheck: true,
            ..CodeConfig::default()
        }
    }

    /// A-3 环境语法类：is_error + 附录 A-1 文案逐字一致。
    #[test]
    fn precheck_flags_async_with_exact_copy() {
        let out = run_code(
            "let x = 1;\n\nasync function f() {}\nreturn x;",
            precheck_cfg(),
        );
        assert!(out.is_error);
        assert_eq!(
            out.content,
            "run_code precheck: 检出环境不支持的语法 \"async\"（第 3 行）——本沙箱是同步环境：没有 async/await/Promise，没有 setTimeout 等 timer，没有 import/fetch。请改同步写法：顺序执行；批量数据在一次执行内用循环 + tools.call 逐次处理。代码未执行，无副作用；修正后重发完整代码。"
        );
    }

    /// A-3 确定性类：附录 A-2 文案逐字一致。
    #[test]
    fn precheck_flags_date_now_with_exact_copy() {
        let out = run_code("let t = Date.now();\nreturn t;", precheck_cfg());
        assert!(out.is_error);
        assert_eq!(
            out.content,
            "run_code precheck: 检出确定性违规 \"Date.now\"（第 1 行）——确定性沙箱不提供 Date.now / Math.random（调用即在运行时抛错，不是返回固定值的桩）。需要时间或随机值时，改用数据记录自带的字段或代码内常量。代码未执行，无副作用；修正后重发完整代码。"
        );
    }

    /// A-3 保留工具类：附录 A-3 文案逐字一致（FD-9 修正后集合——task 在
    /// {run_code, task, todo} 内，__tools_list/__budget 不在）。
    #[test]
    fn precheck_flags_reserved_tool_with_exact_copy() {
        let out = run_code("return tools.call(\"task\", {});", precheck_cfg());
        assert!(out.is_error);
        assert_eq!(
            out.content,
            "run_code precheck: 检出 tools.call(\"task\")（第 1 行）——\"task\" 在沙箱内不可达（设计 D11：防自递归与嵌套成本）。沙箱内只能调用 tools.list() 列出的工具；\"task\" 的调用请移出沙箱、由主循环直接发起。代码未执行，无副作用；修正后重发完整代码。"
        );
    }

    /// FD-11：一次拒答报告全部命中，类间固定序（环境语法 → 确定性 → 保留
    /// 工具），类内按文档序带行号。
    #[test]
    fn precheck_reports_all_classes_in_fixed_order() {
        let out = run_code("let t = Math.random();\nawait f();", precheck_cfg());
        assert!(out.is_error);
        let env = out.content.find("检出环境不支持的语法").unwrap();
        let det = out.content.find("检出确定性违规").unwrap();
        assert!(env < det, "{}", out.content);
        assert!(
            out.content.contains("\"await\"（第 2 行）"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("\"Math.random\"（第 1 行）"),
            "{}",
            out.content
        );
    }

    /// FD-11 封顶：每类列前 20 处，溢出写「等 N 处」（N = 该类总命中数）。
    #[test]
    fn precheck_caps_hits_at_20_with_total() {
        let code = (0..25)
            .map(|i| format!("await f{i}();"))
            .collect::<Vec<_>>()
            .join("\n");
        let msg = precheck(&code).expect("25 awaits must be flagged");
        assert_eq!(msg.matches("（第 ").count(), 20, "{msg}");
        assert!(msg.contains("等 25 处"), "{msg}");
    }

    /// A-3 排序证明 + 零副作用：registry 必坏（白名单含未知工具名）的实例
    /// 上，预检文案仍先于 registry 构建错误返回——构造性证明拒答先于 eval
    /// （wasm 引擎零调用）；scratch 目录零文件（无副作用）。
    #[test]
    fn precheck_fires_before_registry_and_leaves_no_side_effects() {
        let wd = scratch();
        let tool = tool_with_tools(precheck_cfg(), r#"["no_such_tool_xyz"]"#);
        let out = tool
            .run_with_emit(
                &json!({"code": "async function f() {}"}),
                &ctx(&wd),
                &mut |_| {},
            )
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content
                .starts_with("run_code precheck: 检出环境不支持的语法"),
            "{}",
            out.content
        );
        // Sanity: the SAME tool on clean code does hit the broken registry —
        // the rejection above provably never reached eval.
        let clean = tool
            .run_with_emit(&json!({"code": "return 1;"}), &ctx(&wd), &mut |_| {})
            .unwrap();
        assert!(clean.is_error);
        assert!(
            clean.content.contains("registry build failed"),
            "{}",
            clean.content
        );
        assert_eq!(std::fs::read_dir(&wd).unwrap().count(), 0);
    }

    /// A-3 对照：字符串/注释里的 import、await、Date.now 是文本不是语法
    /// （真 lexer 非 substring，需求 v0.2 修订三）。
    #[test]
    fn precheck_ignores_strings_and_comments() {
        let out = run_code(
            "let s = \"no import here\"; // await Date.now in comment\nreturn s;",
            precheck_cfg(),
        );
        assert!(!out.is_error, "{}", out.content);
    }

    /// A-4 诚实边界：混淆写法过预检（预检是效率特性不是安全机制），运行时
    /// 确定性桩照样抛错——两层分工钉死。
    #[test]
    fn precheck_passes_obfuscated_but_runtime_stub_throws() {
        let out = run_code(r#"return globalThis["Date"]["now"]();"#, precheck_cfg());
        assert!(out.is_error, "{}", out.content);
        assert!(
            !out.content.starts_with("run_code precheck:"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("not available in run_code"),
            "{}",
            out.content
        );
    }

    /// A-5：已声明的名字是模型有意自定义（FD-10 声明豁免）；FD-9——
    /// __budget/__tools_list 是可工作的内省拦截，不在检出集合内。
    #[test]
    fn precheck_accepts_declared_fetch_and_budget_introspection() {
        let out = run_code(
            "function fetch(x) { return x; }\nreturn fetch(41) + 1;",
            precheck_cfg(),
        );
        assert!(!out.is_error, "{}", out.content);
        let out = run_code(
            "return typeof tools.call(\"__budget\", {});",
            precheck_cfg(),
        );
        assert!(!out.is_error, "{}", out.content);
    }

    /// A-5 语料：skill md 的全部 js 示例逐块过预检不拦——技能文档教给模型
    /// 的写法必须全部通过预检。
    #[test]
    fn precheck_accepts_all_skill_md_examples() {
        let mut in_block = false;
        let mut block = String::new();
        let mut blocks: Vec<String> = Vec::new();
        for line in RUN_CODE_SKILL_MD.lines() {
            let t = line.trim();
            if !in_block && t.starts_with("```js") {
                in_block = true;
            } else if in_block && t == "```" {
                blocks.push(std::mem::take(&mut block));
                in_block = false;
            } else if in_block {
                block.push_str(line);
                block.push('\n');
            }
        }
        assert!(!blocks.is_empty(), "skill md has no js blocks?");
        for b in &blocks {
            assert!(
                precheck(b).is_none(),
                "skill md example flagged:\n{b}\n→ {}",
                precheck(b).unwrap_or_default()
            );
        }
    }
}
