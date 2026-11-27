#!/usr/bin/env python3
"""Pass condition for the `cjk-docs` regress task.

    python3 check.py <result.json>        (cwd = the run's workdir)

Every rule the task names is checked mechanically against the source document:

* headings    — the first heading is the only H1, every other heading is `## `,
                and the sequence of heading texts is the source's;
* code spans  — the inline code spans are the source's, in the same order and
                at the same places;
* terms       — no listed variant survives outside code spans;
* punctuation — no ASCII `,:;()` next to a Chinese character, no ASCII `.`
                right after one, and (the other way round) no full-width mark
                that is not next to Chinese text;
* content     — with terms mapped and punctuation folded to one width, the
                text outside the code spans equals the source's: nothing was
                added, dropped or reworded.

The term table is checked against the task's own glossary.md first, so the
two cannot drift apart.
"""
import re
import sys
from pathlib import Path

TASK_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(TASK_DIR.parents[1]))
sys.dont_write_bytecode = True  # 不往仓库里落 __pycache__
import checklib  # noqa: E402

DOCS = ("user-guide.md", "install-guide.md")

# 标准写法 → 其它写法. Mirrors fixture/docs/glossary.md (checked in main).
TERMS = {
    "账号": ("帐号", "帐户", "账户"),
    "登录": ("登入", "登陆"),
    "用户": ("使用者",),
    "密码": ("口令",),
    "缓存": ("高速缓存",),
}
FULLWIDTH = {"，": ",", "：": ":", "；": ";", "（": "(", "）": ")", "。": "."}

HAN = re.compile(r"[㐀-䶿一-鿿]")  # a Chinese character
SPAN = re.compile(r"(`+)(.*?)\1(?!`)")
HEADING = re.compile(r"^(#{1,6})(?:\s+(.*))?$")
MARK = "\x00"  # stands for one inline code span


def scan(text):
    """Split a document into (plain lines with a MARK per code span, code span
    contents in order, [(level, text)] headings)."""
    plain, codes, headings = [], [], []
    for line in text.splitlines():
        m = HEADING.match(line)
        if m:
            headings.append((len(m.group(1)), (m.group(2) or "").strip()))
            line = m.group(2) or ""
        found = []

        def take(match):
            found.append(match.group(2))
            return MARK

        plain.append(SPAN.sub(take, line))
        codes += found
    return plain, codes, headings


def glossary_terms():
    """The term table as the task's glossary.md states it."""
    text = (TASK_DIR / "fixture" / "docs" / "glossary.md").read_text(encoding="utf-8")
    terms = {}
    for line in text.splitlines():
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) != 2 or cells[0] in ("标准写法", "") or set(cells[0]) <= set("- "):
            continue
        terms[cells[0]] = tuple(v.strip() for v in re.split("[、,，]", cells[1]) if v.strip())
    return terms


def fold(plain_lines):
    """Text with variants mapped and punctuation folded to ASCII, whitespace
    dropped — the same string for a source and its correct rewrite."""
    text = "\n".join(plain_lines)
    for std, variants in TERMS.items():
        for variant in variants:
            text = text.replace(variant, std)
    for wide, narrow in FULLWIDTH.items():
        text = text.replace(wide, narrow)
    return re.sub(r"\s+", "", text)


def punct_issues(plain_lines):
    issues = []
    for no, line in enumerate(plain_lines, 1):
        for i, ch in enumerate(line):
            before = line[i - 1] if i else ""
            after = line[i + 1] if i + 1 < len(line) else ""
            near_han = bool(HAN.match(before or "") or HAN.match(after or ""))
            if ch in ",:;()" and near_han:
                issues.append(f"line {no}: ASCII {ch!r} next to Chinese text")
            elif ch == "." and HAN.match(before or ""):
                issues.append(f"line {no}: ASCII '.' ending a Chinese sentence")
            elif ch in "，：；（）" and not near_han:
                issues.append(f"line {no}: full-width {ch!r} not next to Chinese text")
            elif ch == "。" and not HAN.match(before or ""):
                issues.append(f"line {no}: '。' does not follow Chinese text")
            elif ch == "`":
                issues.append(f"line {no}: unbalanced backtick")
    return issues


def doc_issues(name, src, out):
    issues = []
    src_plain, src_codes, src_heads = scan(src)
    out_plain, out_codes, out_heads = scan(out)
    levels = [level for level, _ in out_heads]
    if not levels or levels[0] != 1:
        issues.append(f"{name}: the first heading must be the document's single H1")
    wrong = [f"'{'#' * level} {text}'" for level, text in out_heads[1:] if level != 2]
    if wrong:
        issues.append(f"{name}: headings that are not `## `: {', '.join(wrong[:4])}")
    if [t for _, t in out_heads] != [t for _, t in src_heads]:
        issues.append(f"{name}: the heading texts differ from the source")
    if out_codes != src_codes:
        issues.append(f"{name}: inline code spans differ from the source")
    out_text = "\n".join(out_plain)
    left = sorted({v for variants in TERMS.values() for v in variants if v in out_text})
    if left:
        issues.append(f"{name}: non-standard wording left: {', '.join(left)}")
    issues += [f"{name}: {issue}" for issue in punct_issues(out_plain)]
    if fold(out_plain) != fold(src_plain):
        issues.append(f"{name}: content differs from the source once terms and punctuation "
                      f"are folded (something was added, dropped or reworded)")
    return issues


def reasons(workdir):
    issues = checklib.pristine_drift(TASK_DIR, ("docs",))
    for name in DOCS:
        out = workdir / "out" / name
        if not out.is_file():
            issues.append(f"out/{name} is missing")
            continue
        src = (TASK_DIR / "fixture" / "docs" / name).read_text(encoding="utf-8")
        issues += doc_issues(name, src, out.read_text(encoding="utf-8"))
    return issues


if __name__ == "__main__":
    if glossary_terms() != TERMS:
        print("FAIL: check.py's term table no longer matches fixture/docs/glossary.md")
        sys.exit(1)
    sys.exit(checklib.finish(reasons(Path.cwd())))
