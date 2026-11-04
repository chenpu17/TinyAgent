#!/usr/bin/env python3
"""Synchronize the run_code specification and evaluation fingerprints.

No model requests, runtime hooks or benchmark mutations. Run from any directory:
  python3 scripts/sync_run_code_contract.py --write
  python3 scripts/sync_run_code_contract.py --check
"""

import argparse
import hashlib
import json
from pathlib import Path
import re


ROOT = Path(__file__).resolve().parents[1]
APPENDIX = "## 附录 A：LLM 可见契约"
# The generated appendix ends at this marker; anything AFTER it is hand-added
# content that a regeneration would drop — both modes refuse loudly instead.
APPENDIX_END = "<!-- END GENERATED APPENDIX -->"
MANIFEST = ROOT / "docs/run_code_routing_manifest.json"
README_SPECS = {
    "en": ROOT / "README.md",
    "zh": ROOT / "README.zh-CN.md",
}
SPEC_BEGIN_RE = re.compile(r"<!-- BEGIN CODE CONFIG SPEC[^>]*-->\n")
SPEC_END = "<!-- END CODE CONFIG SPEC -->"


def sha(data):
    return hashlib.sha256(data).hexdigest()


def sha_file_rel(path):
    """sha256 of a tracked file's content, line endings folded to LF.

    The manifest pins source files on Linux CI (LF working trees), but a
    Windows checkout with core.autocrlf=true materializes them as CRLF —
    hashing raw bytes would turn the gate into a checkout-format detector
    instead of a content-drift detector. Fold CRLF before hashing so the
    recorded hashes stay platform-independent."""
    return sha((ROOT / path).read_bytes().replace(b"\r\n", b"\n"))


def readme_spec_section(readme):
    """A README's code-config block (JSON example + field table), kept
    between BEGIN/END CODE CONFIG SPEC markers. Both the English and the
    Chinese README carry one — each is cross-checked and hashed."""
    text = readme.read_text(encoding="utf-8")
    m = SPEC_BEGIN_RE.search(text)
    if not m or SPEC_END not in text:
        raise SystemExit(f"{readme.name} is missing the CODE CONFIG SPEC markers.")
    return text[m.end():text.index(SPEC_END)]


def cross_check_readme_spec(section, which):
    """Fail when a README's spec block and config.rs's CodeConfig::default
    disagree on the numeric knobs — the drift direction the file-hash alone
    can't see (changing config.rs touches no hashed file). Runs in BOTH
    --write and --check modes, for BOTH READMEs."""
    src = (ROOT / "crates/core/src/config.rs").read_text(encoding="utf-8")
    dm = re.search(r"impl Default for CodeConfig \{(.*?)\n\}", src, re.S)
    if not dm:
        raise SystemExit("config.rs: `impl Default for CodeConfig` not found (extractor drift)")
    body = dm.group(1)

    def field(name):
        fm = re.search(rf"\b{name}:\s*([0-9_]+)", body)
        if not fm:
            raise SystemExit(f"config.rs: CodeConfig default field {name} not found")
        return int(fm.group(1).replace("_", ""))

    for name in ("max_tool_calls", "mem_mb", "timeout_secs", "fuel", "max_result_chars"):
        value = field(name)
        # Line-exact like the boolean rows below, killing the bare-substring
        # coincidences ("2" greenlit inside a "200" anywhere in the section):
        # the key's own JSON line AND the key's own table row must each carry
        # the exact default as the value cell (backticked or bare; fuel may
        # be spelled 5e9 in the table cell — JSON needs the literal integer).
        if not re.search(rf'^\s*"{name}": {value},?\s*$', section, re.M):
            raise SystemExit(
                f"README[{which}] code-config spec is stale: {name} default is {value} in "
                f"config.rs but the JSON example line does not carry exactly that "
                "value. Update the README block (or revert config.rs), then re-run."
            )
        rows = [ln for ln in section.splitlines() if ln.lstrip().startswith(f"| `{name}`")]
        if len(rows) != 1:
            raise SystemExit(
                f"README[{which}] code-config spec: expected exactly one table row for "
                f"{name}, found {len(rows)} (extractor drift)."
            )
        cell = re.match(r"\|\s*`" + re.escape(name) + r"`\s*\|\s*`?([^`|]+?)`?\s*\|", rows[0].lstrip())
        allowed = [str(value)] + (["5e9"] if value == 5_000_000_000 else [])
        if not cell or cell.group(1).strip() not in allowed:
            raise SystemExit(
                f"README[{which}] code-config spec is stale: {name} default is {value} in "
                f"config.rs but its table row's default cell does not equal one of "
                f"{allowed}. Update the README block (or revert config.rs), then re-run."
            )

    # Boolean/nested knobs (external review round 3): a flipped default here
    # is invisible to BOTH gates above — the numeric check skips bools and
    # config.rs is not a hashed file. The README documents each knob's
    # default in its table row's default column (`key` | `value`).
    def default_of(block: str, name: str) -> str:
        m = re.search(rf"\b{name}:\s*([^,\n]+)", block)
        if not m:
            raise SystemExit(f"config.rs: default field {name} not found (extractor drift)")
        return m.group(1).strip()

    for name in ("enabled", "include", "call_batch", "precheck"):
        value = default_of(body, name)
        if f"`{name}` | `{value}`" not in section:
            raise SystemExit(
                f"README[{which}] code-config spec is stale: {name} default is {value} in "
                "config.rs but the README table row does not carry that default. "
                "Update the README block (or revert config.rs), then re-run."
            )

    # Enum knobs (round-3 review R3-01: the sibling gap of the boolean check —
    # `skill_injection: SkillInjection::OnDemand → Eager` flipped past BOTH
    # older gates because neither extracts enums). The serde label is
    # snake_case (`rename_all`), matching the JSON example line AND the
    # table row's default cell (quoted, like the JSON spelling).
    em = re.search(r"\bskill_injection:\s*SkillInjection::(\w+)", body)
    if not em:
        raise SystemExit("config.rs: default field skill_injection not found (extractor drift)")
    variant = em.group(1)
    ENUM_LABELS = {"Off": "off", "OnDemand": "on_demand", "Eager": "eager"}
    if variant not in ENUM_LABELS:
        raise SystemExit(
            f"config.rs: SkillInjection variant {variant} is not in the extractor's "
            "label map — add it consciously (its serde spelling) before the gate "
            "can check it."
        )
    label = ENUM_LABELS[variant]
    if not re.search(rf'^\s*"skill_injection": "{label}",?\s*$', section, re.M):
        raise SystemExit(
            f"README[{which}] code-config spec is stale: skill_injection default is "
            f'{label} in config.rs but the JSON example line does not carry exactly '
            "that value. Update the README block (or revert config.rs), then re-run."
        )
    rows = [ln for ln in section.splitlines() if ln.lstrip().startswith("| `skill_injection`")]
    if len(rows) != 1:
        raise SystemExit(
            f"README[{which}] code-config spec: expected exactly one table row for "
            f"skill_injection, found {len(rows)} (extractor drift)."
        )
    cell = re.match(
        r"\|\s*`skill_injection`\s*\|\s*`?([^`|]+?)`?\s*\|", rows[0].lstrip()
    )
    if not cell or cell.group(1).strip() != f'"{label}"':
        raise SystemExit(
            f"README[{which}] code-config spec is stale: skill_injection default is "
            f'{label} in config.rs but its table row\'s default cell is not "{label}". '
            "Update the README block (or revert config.rs), then re-run."
        )

    # Vec/array knobs (same R3-01 sweep): the default expression's resolved
    # literal must equal the table row's default cell (compact JSON spelling),
    # and for fs_write_dirs also the JSON example line. A default_*() call is
    # resolved to its function body's string array; any NEW shape must be
    # added consciously or the gate fails loud.
    def vec_default(field_expr: str) -> str:
        if field_expr == "Vec::new()":
            return "[]"
        fm = re.match(r"(\w+)\(\)", field_expr)
        if fm:
            fnm = re.search(rf"fn {fm.group(1)}\(\) -> Vec<String> \{{(.*?)\n\}}", src, re.S)
            if not fnm:
                raise SystemExit(
                    f"config.rs: default fn {fm.group(1)} not found (extractor drift)"
                )
            globs = re.findall(r'"([^"]+)"', fnm.group(1))
            return "[" + ", ".join(f'"{g}"' for g in globs) + "]"
        raise SystemExit(
            f"config.rs: default field expression {field_expr} is not a shape the "
            "extractor resolves (Vec::new() or a default_*() fn) — extend the "
            "extractor consciously."
        )

    for name in ("deny_paths", "fs_write_dirs"):
        vm = re.search(rf"\b{name}:\s*([^,\n]+)", body)
        if not vm:
            raise SystemExit(f"config.rs: default field {name} not found (extractor drift)")
        value = vec_default(vm.group(1).strip())
        rows = [ln for ln in section.splitlines() if ln.lstrip().startswith(f"| `{name}`")]
        if len(rows) != 1:
            raise SystemExit(
                f"README[{which}] code-config spec: expected exactly one table row for "
                f"{name}, found {len(rows)} (extractor drift)."
            )
        cell = re.match(r"\|\s*`" + re.escape(name) + r"`\s*\|\s*`?([^`|]+?)`?\s*\|", rows[0].lstrip())
        if not cell or cell.group(1).strip() != value:
            raise SystemExit(
                f"README[{which}] code-config spec is stale: {name} default resolves to "
                f"{value} in config.rs but its table row's default cell does not equal "
                "exactly that. Update the README block (or revert config.rs), then re-run."
            )
        if name == "fs_write_dirs":
            # The JSON example shows fs_write_dirs' DEFAULT ([]); deny_paths'
            # example intentionally shows a configured value instead.
            if not re.search(r'^\s*"fs_write_dirs": \[\],?\s*$', section, re.M):
                raise SystemExit(
                    f"README[{which}] code-config spec is stale: fs_write_dirs default "
                    "is [] in config.rs but the JSON example line does not carry exactly "
                    "that. Update the README block (or revert config.rs), then re-run."
                )
    # LlmConfig derives Default (no impl block): the nested defaults come
    # from the field types — bool → false, Option → null, Vec → [].
    lm = re.search(r"struct LlmConfig \{(.*?)\n\}", src, re.S)
    if not lm:
        raise SystemExit("config.rs: `struct LlmConfig` not found (extractor drift)")
    lbody = lm.group(1)

    def derived_default(name: str) -> str:
        m = re.search(rf"\b{name}:\s*([A-Za-z\[\]<>:: ]+)", lbody)
        if not m:
            raise SystemExit(f"config.rs: LlmConfig field {name} not found (extractor drift)")
        ty = m.group(1).strip()
        if ty == "bool":
            return "false"
        if ty.startswith("Option"):
            return "null"
        if ty.startswith("Vec"):
            return "[]"
        raise SystemExit(f"config.rs: LlmConfig field {name} has unexpected type {ty}")

    nested = (
        f"enabled:{derived_default('enabled')}, "
        f"default_model:{derived_default('default_model')}, "
        f"models:{derived_default('models')}"
    )
    if nested not in section:
        raise SystemExit(
            f"README[{which}] code-config spec is stale: the `llm` row's nested default "
            f"literal does not match config.rs ({nested}). Update the README "
            "block (or revert config.rs), then re-run."
        )


def residue_after_appendix(text):
    """Anything AFTER the generated appendix's END marker ('' when clean).

    The generated block is defined to END at the marker; content added below
    it would be silently dropped by --write (the appendix is regenerated as
    the file tail), so both modes refuse loudly instead."""
    i = text.find(APPENDIX_END)
    if i < 0:
        return ""
    return text[i + len(APPENDIX_END):].strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--check", action="store_true")
    args = parser.parse_args()

    # run_code.rs was split into run_code/ submodules: the pinned facade
    # path stays, but the extracted bytes moved — description + schema live
    # in tool.rs, PRELUDE_SRC in contract.rs.
    tool_src = (ROOT / "crates/core/src/tools/run_code/tool.rs").read_text(encoding="utf-8")
    matches = re.findall(
        r'name: "run_code"\.into\(\),\s*description: r#"(.*?)"#\s*'
        r'\.into\(\),\s*schema: json!\((\{.*?\})\),', tool_src, re.S
    )
    if len(matches) != 1:
        raise SystemExit("Expected exactly one run_code definition; update the extractor.")
    description, schema = matches[0]
    definition = {"name": "run_code", "description": description, "schema": json.loads(schema)}
    skill = (ROOT / "crates/core/src/tools/run_code_skill.md").read_text(encoding="utf-8")
    teaser = re.search(r"^description: (.+)$", skill, re.M).group(1)
    contract_src = (ROOT / "crates/core/src/tools/run_code/contract.rs").read_text(encoding="utf-8")
    prelude = re.search(r'const PRELUDE_SRC: &str = r#"(.*?)"#;', contract_src, re.S).group(1)
    if len(description) > 1200 or len(teaser) > 1024:
        raise SystemExit(f"Character cap exceeded: description={len(description)}, teaser={len(teaser)}")

    doc = ROOT / "docs/run_code_design.md"
    current = doc.read_text(encoding="utf-8")
    if current.count(APPENDIX) != 1:
        raise SystemExit("Expected exactly one appendix A heading.")
    appendix = (
        APPENDIX + "（由源码生成）\n\n"
        "<!-- Generated by scripts/sync_run_code_contract.py --write; do not edit below; "
        "the generated block ends at the END GENERATED APPENDIX marker. -->\n\n"
        "工具描述、技能正文、设计附录同源；修改后运行同步脚本。"
        "prelude 的封闭助手清单保持与描述及技能一致；脚本记录其哈希，不改运行时。\n\n"
        "**① 工具描述及 schema**（描述 ≤1200 字符，随请求常驻）：\n\n```json\n"
        + json.dumps(definition, ensure_ascii=False, indent=2) + "\n```\n\n"
        "**② 内置技能**（teaser ≤1024 字符；正文按需加载，磁盘同名技能可覆盖）：\n\n````markdown\n"
        + skill.rstrip() + "\n````\n"
        + APPENDIX_END + "\n"
    )
    expected_doc = current.split(APPENDIX, 1)[0] + appendix

    # The appendix is regenerated as the FILE TAIL: anything below its END
    # marker is hand-added content a regeneration would silently drop —
    # refuse in both modes (a doc with no marker yet is pre-migration and
    # passes this probe; the first --write installs the marker).
    residue = residue_after_appendix(current)
    if residue:
        raise SystemExit(
            "content after the generated appendix would be dropped; move it "
            f"above the appendix (first line: {residue.splitlines()[0]!r})"
        )

    paths = [
        "examples/prompts/agentic.md", "examples/prompts/agentic-code.md",
        "crates/core/src/tools/run_code.rs",
        # The split moved the tool body into run_code/ submodules — pin them
        # too so any edit there still forces a conscious manifest refresh.
        # A NEW submodule file must be added here and to gen_structure.py
        # ENTRIES (both gates key on the explicit list).
        "crates/core/src/tools/run_code/contract.rs",
        "crates/core/src/tools/run_code/sandbox.rs",
        "crates/core/src/tools/run_code/guest_api.rs",
        "crates/core/src/tools/run_code/fs_policy.rs",
        "crates/core/src/tools/run_code/lib_index.rs",
        "crates/core/src/tools/run_code/batch.rs",
        "crates/core/src/tools/run_code/precheck.rs",
        "crates/core/src/tools/run_code/tool.rs",
        "crates/core/src/tools/run_code/test_util.rs",
        "crates/core/src/tools/run_code_skill.md", "crates/core/src/tools/skill.rs",
        "docs/finetune_support_design.md",
    ]
    manifest = {
        "contract_version": "run-code-routing-v2",
        "hash_algorithm": "sha256",
        "files": {p: sha_file_rel(p) for p in paths},
        "prompt_bodies": {},
        "description": {"chars": len(description), "sha256": sha(description.encode("utf-8"))},
        "teaser": {"chars": len(teaser), "sha256": sha(teaser.encode("utf-8"))},
        "tool_schema_sha256": sha(json.dumps(definition["schema"], sort_keys=True).encode("utf-8")),
        "prelude_source_sha256": sha(prelude.encode("utf-8")),
    }
    for p in paths[:2]:
        body = (ROOT / p).read_text(encoding="utf-8").split("\n---\n", 1)[1].strip()
        manifest["prompt_bodies"][p] = sha(body.encode("utf-8"))
    # Each README's code-config block: cross-check defaults against config.rs
    # and pin the section's hash (section-scoped so unrelated README edits —
    # e.g. the generated structure tree — don't churn the manifest). Both
    # language editions carry the block; either may drift alone, so both are
    # gated and pinned under per-language keys.
    readme_code_spec = {}
    for which, readme in README_SPECS.items():
        spec = readme_spec_section(readme)
        cross_check_readme_spec(spec, which)
        readme_code_spec[which] = {"sha256": sha(spec.encode("utf-8"))}
    manifest["readme_code_spec"] = readme_code_spec
    expected_manifest = json.dumps(manifest, ensure_ascii=False, indent=2) + "\n"

    if args.write:
        # newline="\n" keeps the generated files byte-identical across
        # platforms (text-mode default would write CRLF on Windows).
        doc.write_text(expected_doc, encoding="utf-8", newline="\n")
        MANIFEST.write_text(expected_manifest, encoding="utf-8", newline="\n")
    else:
        if current != expected_doc:
            raise SystemExit("Appendix A is stale; run with --write.")
        if not MANIFEST.exists() or MANIFEST.read_text(encoding="utf-8") != expected_manifest:
            raise SystemExit("Routing manifest is stale; run with --write.")
    print(json.dumps(manifest, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
