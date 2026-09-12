#!/usr/bin/env python3
"""Run selected on-demand routing probes through the sibling benchmark.

Default is a dry run. --run reuses completed matching runs; it never forces
reruns or modifies benchmark source files / historical arm results.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import hashlib
import json
from pathlib import Path
import sys


ROOT = Path(__file__).resolve().parents[1]
# awlikesong/awclean: 正例探针对；awlikes/awphoto: 负例对照（小集合，验误触发）
TASKS = ("awlikesong", "awclean", "awlikes", "awphoto")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bench-root", type=Path, default=ROOT.parent / "TinyAgentBench")
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--tasks", nargs="+", choices=TASKS, default=list(TASKS))
    parser.add_argument("--repeats", type=int, default=1,
                        help="repeats per task; >1 derives an r<N> arm suffix")
    parser.add_argument("--arm-suffix", default="",
                        help="override the derived arm suffix (disambiguate task subsets)")
    args = parser.parse_args()
    sys.path.insert(0, str(args.bench_root.resolve()))
    from bench import arms, config, runner

    manifest = json.loads((ROOT / "docs/run_code_routing_manifest.json").read_text())
    version = manifest["contract_version"].removeprefix("run-code-")
    if args.arm_suffix:
        suffix = args.arm_suffix
    elif args.repeats > 1:
        suffix = f"r{args.repeats}"
    else:
        suffix = ""
    arm_name = f"ta-code-{version}{('-' + suffix) if suffix else ''}-aw"
    selected_tasks = list(dict.fromkeys(args.tasks))
    for path, expected in manifest["files"].items():
        if hashlib.sha256((ROOT / path).read_bytes()).hexdigest() != expected:
            raise SystemExit(f"Contract fingerprint mismatch: {path}")
    prompt_path = ROOT / "examples/prompts/agentic-code.md"
    raw = prompt_path.read_text()
    prompt_sha = hashlib.sha256(prompt_path.read_bytes()).hexdigest()
    body = raw.split("\n---\n", 1)[1].strip()
    original_spec = arms.ta_spec

    def pilot_spec(model, arm):
        spec = original_spec(model, arm)
        if arm == arm_name:
            spec["system_prompt"] = body + "\n\n" + arms.TA_ANSWER_CONTRACT
            spec["system_prompt_source"] = {
                "file": str(prompt_path), "sha256": prompt_sha,
                "tail": "unchanged bench answer contract",
            }
            spec["routing_contract"] = manifest
            spec["code_skill_injection"] = "on_demand"
        return spec

    arms.ta_spec = pilot_spec
    if arm_name not in arms.ALL_ARMS:
        arms.ALL_ARMS.append(arm_name)
    gw = config.gateway()  # Never serialize the gateway credential.
    repeats = list(range(1, args.repeats + 1))
    metadata = {
        "arm": arm_name, "tasks": selected_tasks, "repeats": repeats, "model": gw["model"],
        "skill_injection": "on_demand", "system_prompt_sha256": prompt_sha,
        "binary": str(config.TINYAGENT_BIN),
        "binary_sha256": hashlib.sha256(config.TINYAGENT_BIN.read_bytes()).hexdigest(),
        "contract": manifest,
    }
    print(json.dumps({k: v for k, v in metadata.items() if k != "contract"}, indent=2), flush=True)
    if not args.run:
        return
    output = config.RUNS / arm_name
    output.mkdir(parents=True, exist_ok=True)
    metadata_path = output / "pilot.json"
    if metadata_path.exists() and json.loads(metadata_path.read_text()) != metadata:
        raise SystemExit("Pilot conditions changed; preserve this batch and choose a new arm name.")
    metadata_path.write_text(json.dumps(metadata, ensure_ascii=False, indent=2) + "\n")
    runner.ensure_proxy()
    with ThreadPoolExecutor(max_workers=2) as pool:
        jobs = {pool.submit(runner.run_combo, arm_name, tag, k, gw): (tag, k)
                for tag in selected_tasks for k in repeats}
        for future in as_completed(jobs):
            tag, k = jobs[future]
            result = future.result()
            print(json.dumps({"task": tag, "repeat": k, **{k2: result.get(k2) for k2 in
                ("complete", "skipped", "verdict", "wall_segs", "proxy", "client_usage")}},
                ensure_ascii=False), flush=True)


if __name__ == "__main__":
    main()
