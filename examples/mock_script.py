#!/usr/bin/env python3
"""Scripted mode shared by the two example LLM mocks.

`examples/mock_openai.py` / `examples/mock_anthropic.py` answer two ways. On
their own: a fixed two-step behavior (one `write_file` call, then a final
text). With `--script FILE`: they play a model that follows a fixed script —
one entry per chat request, in order — so a whole task can be replayed through
the real CLI with no model and no cost. That is the zero-cost check of
`scripts/regress_eval.py` (docs/kernel_improvement_plan.md §0.4).

The script is wire-neutral (both mocks read the same file and each renders it
in its own protocol's SSE events):

    {"turns": [
      {"tools": [{"name": "read_file", "arguments": {"path": "a.py"}}],
       "reasoning": "optional thinking text, streamed before the calls",
       "usage": {"prompt": 1200, "completion": 40, "reasoning": 0, "cached": 600}},
      {"text": "final answer"}
    ]}

- Every key but `tools` / `text` is optional; a turn may carry both.
- Requests past the last turn repeat it (the run still ends — on the turn cap
  or the kernel's stuck-loop guard).
- `usage` defaults to a deterministic function of the request and response
  sizes: a run with fixed inputs reports the same bill twice.
- A compression summary request (system prompt starting with the kernel's
  SUMMARY_SYSTEM) never consumes a turn: it gets a fixed summary text.

The mocks import this module lazily, only in scripted mode, so each mock still
runs as a single copied file.
"""
import json

# crates/core/src/compress.rs `SUMMARY_SYSTEM`. A prefix match: the kernel may
# append guidance to the shared prompt.
SUMMARY_PREFIX = "You are compressing the earlier part of an AI agent's conversation"
SUMMARY_TEXT = (
    "[mock summary] The scripted mock model is following its script. The task "
    "instructions in the first message and anything already written to files "
    "under the workdir remain authoritative; continue with the next step."
)

USAGE_KEYS = ("prompt", "completion", "reasoning", "cached")


class Script:
    """One scripted run. Each chat request consumes the next turn; requests
    past the end repeat the last one."""

    def __init__(self, turns):
        self._validate(turns)
        self.turns = turns
        self.count = 0

    @staticmethod
    def _validate(turns):
        def bad(msg):
            raise ValueError(f"script: {msg}")

        if not isinstance(turns, list) or not turns:
            bad('"turns" must be a non-empty list')
        for n, turn in enumerate(turns):
            where = f"turn {n}"
            if not isinstance(turn, dict):
                bad(f"{where} must be an object")
            unknown = sorted(set(turn) - {"tools", "text", "reasoning", "usage"})
            if unknown:
                bad(f"{where} has unknown key(s) {unknown}")
            if "tools" not in turn and "text" not in turn:
                bad(f'{where} needs "tools" or "text"')
            for k in ("text", "reasoning"):
                if k in turn and not isinstance(turn[k], str):
                    bad(f'{where}: "{k}" must be a string')
            calls = turn.get("tools")
            if calls is not None:
                if not isinstance(calls, list) or not calls:
                    bad(f'{where}: "tools" must be a non-empty list')
                for call in calls:
                    if (not isinstance(call, dict) or set(call) != {"name", "arguments"}
                            or not isinstance(call["name"], str) or not call["name"]
                            or not isinstance(call["arguments"], dict)):
                        bad(f'{where}: every tool call needs a "name" and an '
                            '"arguments" object, nothing else')
            usage = turn.get("usage")
            if usage is not None:
                if not isinstance(usage, dict):
                    bad(f'{where}: "usage" must be an object')
                for k, v in usage.items():
                    if k not in USAGE_KEYS:
                        bad(f'{where}: "usage" has unknown key {k!r}')
                    if isinstance(v, bool) or not isinstance(v, int) or v < 0:
                        bad(f'{where}: "usage.{k}" must be an integer >= 0')

    def next_turn(self):
        turn = self.turns[min(self.count, len(self.turns) - 1)]
        self.count += 1
        return turn


def load_script(path):
    with open(path, encoding="utf-8") as f:
        doc = json.load(f)
    if not isinstance(doc, dict) or "turns" not in doc:
        raise ValueError(f"script {path}: top level must be an object with a \"turns\" list")
    return Script(doc["turns"])


def _system_texts(body):
    texts = []
    system = body.get("system")
    if isinstance(system, str):
        texts.append(system)
    elif isinstance(system, list):
        for part in system:
            if isinstance(part, str):
                texts.append(part)
            elif isinstance(part, dict):
                texts.append(str(part.get("text", "")))
    for msg in body.get("messages") or []:
        if msg.get("role") in ("system", "developer"):
            content = msg.get("content")
            if isinstance(content, str):
                texts.append(content)
            elif isinstance(content, list):
                texts.extend(str(p.get("text", "")) for p in content if isinstance(p, dict))
    return texts


def is_summary(body):
    """True for a compression summary request (both wire formats)."""
    return any(t.startswith(SUMMARY_PREFIX) for t in _system_texts(body))


def usage_for(body, reply_chars=0, n_calls=0, reasoning_chars=0, override=None):
    """Token bill of one response, as {"prompt", "completion", "reasoning", "cached"}.

    `override` (a turn's "usage") fixes any axis; the others are estimated
    from sizes, so the bill depends only on the request and the reply.
    """
    # The prompt bill covers what a real provider bills: system, tool
    # definitions and messages.
    sized = {"system": body.get("system"), "tools": body.get("tools"),
             "messages": body.get("messages") or []}
    u = {
        "prompt": max(16, len(json.dumps(sized, ensure_ascii=False)) // 4),
        "completion": max(8, (reply_chars + reasoning_chars) // 4 + 12 * n_calls),
        "reasoning": reasoning_chars // 4,
        # A later request of one run re-sends a prefix of the previous one:
        # report half of it as a cache read (the first request reads nothing).
        "cached": 0,
    }
    if any(m.get("role") == "tool" or _has_tool_result(m) for m in body.get("messages") or []):
        u["cached"] = u["prompt"] // 2
    if override:
        u.update(override)
    # The reported axes keep their real-world relations: cached is a subset of
    # the prompt, reasoning a subset of the completion.
    u["cached"] = min(u["cached"], u["prompt"])
    u["reasoning"] = min(u["reasoning"], u["completion"])
    return u


def _has_tool_result(msg):
    content = msg.get("content")
    return isinstance(content, list) and any(
        isinstance(b, dict) and b.get("type") == "tool_result" for b in content
    )
