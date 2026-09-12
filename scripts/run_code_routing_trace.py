#!/usr/bin/env python3
"""Decompose archived routing-pilot runs (sibling TinyAgentBench repo).

Reads stdout.txt + proxy.jsonl + wd/result.json of one run directory and
prints: outer-turn table (aligned via per-request message-count increments),
per-request token/duration billing, and cache-state decomposition.

Offline analysis only; never calls a model or touches benchmark state.
"""
import argparse
import json
import re
from pathlib import Path

CALL = re.compile(r"^\[tool →\] (\S+) ")
OK = re.compile(r"^\[tool ✓\] (\S+): ")
ERR = re.compile(r"^\[tool ✗\] (\S+): ")


def parse_stdout(text):
    """Split stdout into events; classify each as narr/call/result.

    Sandbox-internal tool calls are logged in the same format as outer ones,
    so stdout alone cannot delimit outer turns — result.json provides the
    outer-call ground truth and proxy.jsonl the turn count.
    """
    events = []
    for line in text.splitlines():
        m = CALL.match(line)
        if m:
            events.append(("call", m.group(1)))
            continue
        m = OK.match(line) or ERR.match(line)
        if m:
            events.append(("result", m.group(1)))
            continue
        if line.strip():
            events.append(("narr", line.strip()))
    return events


def segment_outer_turns(outer_calls, msgs):
    """Group the ordered outer-call list into turns using the per-request
    message counts: response t's calls are those whose results appear by
    request t+1, i.e. 1 + calls_t = msgs[t+1] - msgs[t]."""
    turns = [[] for _ in msgs]
    idx = 0
    for t in range(len(msgs)):
        budget = (msgs[t + 1] - msgs[t] - 1) if t + 1 < len(msgs) else len(outer_calls) - idx
        while budget > 0 and idx < len(outer_calls):
            turns[t].append(outer_calls[idx])
            idx += 1
            budget -= 1
    return turns


def classify(call_names):
    if "read_skill" in call_names:
        return "read_skill"
    if "run_code" in call_names:
        return "run_code"
    if any("complete_task" in n for n in call_names):
        return "finish"
    if call_names and all(n == "todo" for n in call_names):
        return "todo"
    reads = {"show", "get", "search", "list", "read"}
    if call_names and all(any(k in n for k in reads) for n in call_names):
        return "direct_read"
    return "direct_act" if call_names else "narrative"


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("run_dir", type=Path)
    args = ap.parse_args()
    d = args.run_dir
    res = json.loads((d / "wd" / "result.json").read_text())
    proxy = [json.loads(l) for l in (d / "proxy.jsonl").read_text().strip().splitlines()]
    outer = [(c["name"], c.get("input") or {}) for c in res["tool_calls"]]
    turns = segment_outer_turns([n for n, _ in outer], [p["messages"] for p in proxy])

    print(f"== {d}")
    print(f"verdict_from_run_json=see run.json  turns={res['turns']}  requests={len(proxy)}  outer_calls={len(outer)}")
    pos = 0
    for t, names in enumerate(turns):
        meta = []
        for n in names:
            _, inp = outer[pos]
            pos += 1
            if "page_limit" in inp:
                meta.append(f"{n.replace('mcp_appworld_', '')}[{inp.get('page_index')}/{inp['page_limit']}]")
            elif n == "run_code":
                meta.append(f"run_code[{len(inp.get('code') or '')}c]")
            else:
                meta.append(n.replace("mcp_appworld_", ""))
        u = proxy[t]["usage"]
        det = u.get("prompt_tokens_details") or {}
        print(f"T{t + 1:2d} {classify(names):11s} out={u['completion_tokens']:5d} "
              f"unc={u['prompt_tokens'] - det.get('cached_tokens', 0):6d} "
              f"dur={proxy[t]['dur_ms'] / 1000:6.1f}s  {', '.join(meta) or '(narrative)'}")

    req1 = proxy[0]["usage"]
    det = req1.get("prompt_tokens_details") or {}
    req1_unc = req1["prompt_tokens"] - det.get("cached_tokens", 0)
    total_unc = sum(p["usage"]["prompt_tokens"]
                    - (p["usage"].get("prompt_tokens_details") or {}).get("cached_tokens", 0)
                    for p in proxy)
    print(f"\nreq1_uncached={req1_unc}  (>1k ⇒ cold start; <2k ⇒ warm provider cache)")
    print(f"sum_uncached={total_unc}  conversation_growth={total_unc - req1_unc}  output={res['output_tokens']}")


if __name__ == "__main__":
    main()
