#!/usr/bin/env python3
"""Drift gate for the E0' eval apparatus (M4, docs/code_llm_eval_assembly.md).

Checks the experiment-side skill variant stays what the assembly doc
registered — the checks a stale/forgotten edit would silently break:

  1. fingerprint: sha256(examples/eval/e0prime-skill/run-code/SKILL.md)
     equals the digest recorded in the assembly doc;
  2. boundary sentence: the frozen A3 block in docs/code_llm_design.md
     appears VERBATIM in the variant (wording changes must go through the
     design doc's revision log, then be mirrored here);
  3. gated-content leak: the variant carries no gate-scoped API teaching
     (callBatch / saveLib / include helpers / gated section headers) — with
     all gates off the model-visible contract must stay all-off;
  4. frontmatter: `name` equals the parent dir name (the skills scan
     requirement), description non-empty.

  python3 scripts/check_eval_apparatus.py   # exits non-zero on any drift
"""

import hashlib
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
VARIANT = ROOT / "examples/eval/e0prime-skill/run-code/SKILL.md"
ASSEMBLY = ROOT / "docs/code_llm_eval_assembly.md"
DESIGN = ROOT / "docs/code_llm_design.md"


def fail(msg):
    print(f"[apparatus] {msg}")
    sys.exit(1)


def main():
    if not VARIANT.is_file():
        fail(f"missing {VARIANT.relative_to(ROOT)}")
    text = VARIANT.read_text(encoding="utf-8")
    # Hash CRLF folded to LF (same rationale as sha_file_rel in
    # scripts/sync_run_code_contract.py): the fingerprint was registered from
    # an LF working tree, but a Windows checkout with core.autocrlf=true
    # materializes the file as CRLF — hashing raw bytes would turn this gate
    # into a checkout-format detector instead of a content-drift detector.
    digest = hashlib.sha256(
        VARIANT.read_bytes().replace(b"\r\n", b"\n")
    ).hexdigest()

    # 1. fingerprint registered in the assembly doc.
    asm = ASSEMBLY.read_text(encoding="utf-8")
    m = re.search(r"指纹（sha256，全 64 位）[^`]*```\n([0-9a-f]{64})\n```", asm)
    if not m:
        fail("assembly doc: fingerprint block not found (extractor drift)")
    if m.group(1) != digest:
        fail(
            f"variant drift: sha256 is {digest} but the assembly doc "
            f"registered {m.group(1)} — update the doc (and note WHY in its "
            "revision log), or revert the variant."
        )

    # 2. the frozen A3 boundary block appears verbatim in the variant.
    design = DESIGN.read_text(encoding="utf-8")
    dm = re.search(
        r"\*\*③ 统一三选一边界句[^*]*\*\*.*?```\n(.*?)```", design, re.S
    )
    if not dm:
        fail("design doc: appendix A③ block not found (extractor drift)")
    block = dm.group(1).strip("\n")
    if block not in text:
        fail(
            "boundary sentence drift: the frozen A③ block is not verbatim "
            "in the E0' variant — wording changes go through the design "
            "doc's revision log first, then mirror the file."
        )

    # 3. no gate-scoped API teaching in the all-off variant.
    for leaked in ("callBatch", "saveLib", "【库】", "【批量】", "【LLM 判别】",
                   "BEGIN LIB SECTION", "BEGIN BATCH SECTION", "BEGIN LLM SECTION"):
        if leaked in text:
            fail(f"gated-content leak in the all-off variant: '{leaked}'")

    # 4. frontmatter: name == parent dir (the skills-scan requirement).
    fm = re.match(r"^---\n(.*?)\n---\n", text, re.S)
    if not fm:
        fail("variant: no frontmatter block")
    nm = re.search(r"^name:\s*(\S+)", fm.group(1), re.M)
    ds = re.search(r"^description:\s*(.+)", fm.group(1), re.M)
    if not nm or nm.group(1) != "run-code":
        fail("variant: frontmatter name must be 'run-code' (== parent dir)")
    if not ds or not ds.group(1).strip():
        fail("variant: frontmatter description is empty")

    print(f"[apparatus] OK — variant sha256 {digest[:16]}…, A③ verbatim, no gate leaks.")


if __name__ == "__main__":
    main()
