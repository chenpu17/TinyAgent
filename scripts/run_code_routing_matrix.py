#!/usr/bin/env python3
"""Four-arm routing matrix through the sibling benchmark (fresh campaign).

Arms (AppWorld 17 tasks x 3 repeats each, one fixed-seed interleaved shuffle):

  ta-mx-old-aw        rebuilt old contract (276fb75) + agentic.md     + on_demand
  ta-mx-old-eager-aw  same binary + agentic.md                        + eager
  ta-mx-v2-aw         v2 contract + agentic-code.md                   + on_demand
  ta-mx-v2-eager-aw   same binary + agentic-code.md                   + eager

The old-contract binary is REBUILT from source 276fb75 (build is not
bit-reproducible); it is a new baseline batch, not the historical one, and
historical arms are never touched. Binary selection is keyed on the arm
argument (no shared mutable state); fingerprints follow via a patched
runner._client_id. Restartable: completed runs with matching fingerprints
are skipped.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import hashlib
import json
import os
from pathlib import Path
import random
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
OLD_BIN = Path("/tmp/ta-old-build/target/release/tiny-agent")
V2_BIN = ROOT / "target" / "release" / "tiny-agent"
ARM_MATRIX = {
    "ta-mx-old-aw": "old",
    "ta-mx-old-eager-aw": "old",
    "ta-mx-v2-aw": "v2",
    "ta-mx-v2-eager-aw": "v2",
}


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--bench-root", type=Path, default=ROOT.parent / "TinyAgentBench")
    ap.add_argument("--run", action="store_true")
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--seed", type=int, default=20260911)
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--arms", nargs="+", default=list(ARM_MATRIX))
    ap.add_argument("--tasks", nargs="+", default=None, help="default: all AppWorld tasks")
    args = ap.parse_args()
    for a in args.arms:
        if a not in ARM_MATRIX:
            raise SystemExit(f"unknown arm {a}")
    sys.path.insert(0, str(args.bench_root.resolve()))
    from bench import appworld_defs as aw_defs
    from bench import arms, config, runner

    manifest = json.loads((ROOT / "docs/run_code_routing_manifest.json").read_text())
    if manifest["contract_version"] != "run-code-routing-v2":
        raise SystemExit(f"manifest is {manifest['contract_version']}, matrix expects v2")
    for path, expected in manifest["files"].items():
        if hashlib.sha256((ROOT / path).read_bytes()).hexdigest() != expected:
            raise SystemExit(f"Contract fingerprint mismatch: {path}")
    for b in (OLD_BIN, V2_BIN):
        if not b.exists():
            raise SystemExit(f"missing binary: {b}")
    bin_sha = {k: hashlib.sha256(p.read_bytes()).hexdigest()
               for k, p in (("old", OLD_BIN), ("v2", V2_BIN))}

    prompt_path = ROOT / "examples/prompts/agentic-code.md"
    raw = prompt_path.read_text()
    prompt_sha = hashlib.sha256(prompt_path.read_bytes()).hexdigest()
    body = raw.split("\n---\n", 1)[1].strip()
    tasks = args.tasks or list(aw_defs.AW_TASKS)

    original_spec = arms.ta_spec
    original_client_id = runner._client_id

    def matrix_spec(model, arm):
        spec = original_spec(model, arm)
        if arm.startswith("ta-mx-v2"):
            spec["system_prompt"] = body + "\n\n" + arms.TA_ANSWER_CONTRACT
            spec["system_prompt_source"] = {
                "file": str(prompt_path), "sha256": prompt_sha,
                "tail": "unchanged bench answer contract",
            }
            spec["routing_contract"] = {"version": manifest["contract_version"],
                                        "manifest_files": manifest["files"]}
            label = "eager" if arm.endswith("-eager-aw") else "on_demand"
            spec["code_skill_injection"] = f"{label} (routing-v2 + agentic-code.md)"
        return spec

    def matrix_client_id(arm):
        if arm in ARM_MATRIX:
            return bin_sha[ARM_MATRIX[arm]][:16]
        return original_client_id(arm)

    def run_ta_armkeyed(arm, run_dir, prompt, model, aw_apis_url=None,
                        key=arms.DUMMY_KEY):
        # verbatim run_ta for the -aw path, with the binary keyed on the arm
        assert arm in ARM_MATRIX and arm.endswith("-aw"), arm
        wd = run_dir / "wd"
        spec = matrix_spec(model, arm)
        cfg = {
            "provider": spec["provider"], "base_url": spec["base_url"] + "/v1",
            "api_key_env": "TA_BENCH_KEY", "model": model,
            "max_tokens": spec["max_tokens"], "system_prompt": spec["system_prompt"],
            "tools": spec["tools"],
            "code": ({"enabled": True, "skill_injection": "eager"}
                     if arm.endswith("-eager-aw") else {"enabled": True}),
            "limits": dict(spec["limits"]),
            "mcp_servers": [aw_defs.mcp_config(wd.parent.parent.name, aw_apis_url)],
        }
        (wd / "cfg.json").write_text(json.dumps(cfg, indent=1, ensure_ascii=False))
        arms.TA_XDG.mkdir(parents=True, exist_ok=True)
        env = dict(os.environ, TA_BENCH_KEY=key, NO_COLOR="1",
                   XDG_CONFIG_HOME=str(arms.TA_XDG))
        bin_path = OLD_BIN if ARM_MATRIX[arm] == "old" else V2_BIN
        cmd = [str(bin_path), "--config", "cfg.json", "--task", prompt,
               "--output", "result.json", "--session", "none"]
        ec, killed = arms._exec_logged(cmd, wd, env, run_dir / "stdout.txt",
                                       run_dir / "stderr.txt", config.CLIENT_TIMEOUT)
        return {"exit": ec, "killed": killed,
                "cmd": f"{bin_path.name}(mx) --config cfg.json --task …"}

    arms.ta_spec = matrix_spec
    arms.run_ta = run_ta_armkeyed
    runner._client_id = matrix_client_id
    for a in ARM_MATRIX:
        if a not in arms.ALL_ARMS:
            arms.ALL_ARMS.append(a)

    gw = config.gateway()  # Never serialize the gateway credential.
    jobs = [(a, t, k) for a in args.arms for t in tasks
            for k in range(1, args.repeats + 1)]
    random.Random(args.seed).shuffle(jobs)
    campaign = {
        "campaign": "routing-matrix-v2", "ts": time.time(), "seed": args.seed,
        "model": gw["model"], "repeats": args.repeats, "tasks": tasks,
        "arms": {a: {"bin": ARM_MATRIX[a], "bin_sha256": bin_sha[ARM_MATRIX[a]],
                     "prompt": "agentic-code.md" if a.startswith("ta-mx-v2") else "agentic.md"}
                 for a in args.arms},
        "prompt_sha256": prompt_sha, "manifest": manifest,
        "old_binary_note": "rebuilt from 276fb75 (not bit-reproducible; new baseline batch)",
        "old_bin_source": "git 276fb758db0bbb1e1a8eac4baff1cf7d7098c6cb",
    }
    print(json.dumps({k: v for k, v in campaign.items()
                      if k not in ("manifest", "old_binary_note")}, ensure_ascii=False, indent=2))
    print(f"jobs: {len(jobs)} (interleaved, seed={args.seed})")
    if not args.run:
        return
    meta_path = config.RUNS / "routing-matrix-v2-campaign.json"
    if meta_path.exists() and json.loads(meta_path.read_text())["seed"] != args.seed:
        raise SystemExit("Campaign metadata exists with another seed; inspect before rerun.")
    meta_path.write_text(json.dumps(campaign, ensure_ascii=False, indent=2) + "\n")
    runner.ensure_proxy()
    done = failed = 0
    t0 = time.time()
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        submits = {pool.submit(runner.run_combo, a, t, k, gw): (a, t, k) for a, t, k in jobs}
        for future in as_completed(submits):
            a, t, k = submits[future]
            try:
                result = future.result()
                verdict = result.get("verdict") if isinstance(result, dict) else result
                skipped = isinstance(result, dict) and result.get("skipped")
                done += 1
                print(f"[{done}/{len(jobs)}] {a}/{t}/r{k} -> {verdict}"
                      f"{' (skipped-cache)' if skipped else ''} elapsed={time.time()-t0:.0f}s",
                      flush=True)
            except Exception as exc:  # noqa: BLE001 — log and keep the campaign going
                failed += 1
                print(f"[FAIL {failed}] {a}/{t}/r{k}: {exc!r}", flush=True)
    print(f"campaign finished: {done} done, {failed} failed, "
          f"{(time.time() - t0) / 60:.1f} min", flush=True)


if __name__ == "__main__":
    main()
