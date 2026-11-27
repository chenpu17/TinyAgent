#!/usr/bin/env python3
"""On-the-wire verification of the Anthropic-protocol reasoning carry.

TinyAgent echoes the provider-signed `thinking` blocks of the running turn
back in every later request of that turn ("reasoning carry",
`preserved_reasoning`). The in-process tests pin the kernel's bookkeeping;
this script pins what actually leaves the process — request by request, byte
for byte — through the real headless binary:

  check   (default; the CI gate — offline, deterministic, a few seconds)
          An in-process scripted Messages API mock on 127.0.0.1 records every
          request body and answers scripted SSE. Scenarios: the block shapes
          seen on real traffic, carry off, thinking disabled, an explicit
          effort, a multi-turn conversation (the lifetime rule), a
          cross-process `--resume`, the max_tokens wrap-up turn (a new user
          turn ending the carry), and a compression fold.
  capture (manual; never in CI)
          A localhost forwarding proxy that records every request body and
          reassembles every streamed response, so a run against a real
          endpoint can be audited afterwards. Credential hygiene: credential-
          looking headers are recorded by NAME only, and everything written is
          scanned for key patterns (a hit is redacted and fails loudly).
  audit   (offline)
          Re-runs the same checks over a capture directory, with the session
          file and result.json as optional second and third witnesses.

The three subcommands share every checking function in this file (SSE
reassembly, shape rules, tool_use/tool_result pairing, echo comparison, usage
math, the session three-way) — nothing is implemented twice. A mutation
self-test feeds tampered copies (signature altered, text altered, position
moved, block dropped, tool input changed) through the same comparisons and
requires a mismatch.

Output: one `[PASS] <check> — <what was pinned>` or `[FAIL] …` line per check
and a final summary line; exit code 0 iff nothing failed. `[GAP]` marks a
check that fails only for a reason already filed as a kernel gap
(`KNOWN_GAPS`): it is reported, not counted, unless `--strict` (or
`TA_WIRE_STRICT=1`) is set.

Usage:
    python3 scripts/e2e_anthropic_wire.py                   # check (CI gate)
    python3 scripts/e2e_anthropic_wire.py check --verbose   # per-request detail
    python3 scripts/e2e_anthropic_wire.py check --only compression
    python3 scripts/e2e_anthropic_wire.py capture --upstream <base-url> \\
        --listen 127.0.0.1:8787 --out /tmp/ta-wire-capture
    python3 scripts/e2e_anthropic_wire.py audit /tmp/ta-wire-capture \\
        --session <session.json> --result <result.json>
    TA_BIN=/path/to/tiny-agent python3 scripts/e2e_anthropic_wire.py

No network beyond 127.0.0.1, no API key, no third-party module (3.9+).
"""
from __future__ import annotations

import argparse
import base64
import codecs
import datetime
import hashlib
import http.client
import json
import os
import re
import shutil
import signal
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import traceback
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

sys.dont_write_bytecode = True

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("TA_BIN", os.path.join(REPO, "target", "debug", "tiny-agent"))

# A made-up variable name: the child gets a dummy value through it, nothing else does.
KEY_ENV = "TA_WIRE_KEY"
ANTHROPIC_VERSION = "2023-06-01"
AXES = ("input_tokens", "output_tokens", "cache_creation_input_tokens", "cache_read_input_tokens")
REASONING_TYPES = ("thinking", "redacted_thinking")
THINKING_KEYS = {"type", "thinking", "signature"}
REDACTED_KEYS = {"type", "data"}
PROXY_VARS = {"http_proxy", "https_proxy", "all_proxy", "ftp_proxy", "no_proxy"}

# A check that fails only for a reason filed here prints [GAP] and is not
# counted as a failure: the gap is the kernel's and already known, not a
# regression of what the checker pins. --strict turns it into a red check.
#   {"<rule>": "<where the kernel falls short, one line>"}
# <rule> is the first element of the (rule, message) pairs the checkers return
# and must be one the failing check declares with gaps=(...). A full run in
# which a filed rule never shows up fails ("it no longer reproduces"): a gap
# fixed in the kernel must be taken out of this table right away, in the same
# commit as the fix. Nothing is filed — the first entry, headless
# `--resume --task` carrying the finished turn's thinking into the new turn,
# was fixed in the kernel; the multi-turn scenario pins it as a plain check.
KNOWN_GAPS = {}

STRICT = os.environ.get("TA_WIRE_STRICT", "").lower() not in ("", "0", "false", "no")


def utcnow():
    now = datetime.datetime.now(datetime.timezone.utc)
    return now.strftime("%Y-%m-%dT%H:%M:%S.") + "%03dZ" % (now.microsecond // 1000)


def _str(value):
    return value if isinstance(value, str) else ""


# ── credential hygiene ────────────────────────────────────────────────────────

NAME_ONLY = "<not recorded: credential-looking header name>"
SECRET_HEADER = re.compile(r"api-?key|auth|token|key|secret|cookie|credential|session|bearer", re.I)
# The first two are the repository's own leak-scan patterns; the third catches
# a key pasted into a JSON field (a tool result that read a config file, say).
SK_KEY = re.compile(r"sk-[A-Za-z0-9]{20,}")
BEARER = re.compile(r"Bearer +[A-Za-z0-9._~+/=-]{20,}")
KEY_FIELD = re.compile(
    r"(\"?(?:api[_-]?key|access[_-]?token|refresh[_-]?token|auth[_-]?token|"
    r"client[_-]?secret|secret|password)\"?\s*[:=]\s*\"?)([A-Za-z0-9._~+/=-]{16,})",
    re.I)
SECRET_PATTERNS = (("sk-key", SK_KEY), ("bearer", BEARER), ("key-field", KEY_FIELD))


def find_secrets(text):
    """Names of the key patterns that occur in the text — never the match."""
    return sorted({name for name, pattern in SECRET_PATTERNS if pattern.search(text)})


def scrub(text):
    """Rewrite every key-pattern hit in place (a `"<field>": "<value>"` hit
    keeps its field name)."""
    text = SK_KEY.sub("<redacted:sk-key>", text)
    text = BEARER.sub("<redacted:bearer>", text)
    return KEY_FIELD.sub(lambda match: match.group(1) + "<redacted:key-field>", text)


def safe_headers(headers):
    """Header names → values for the record: credential-looking names are kept
    by NAME only, every other value is leak-guarded."""
    out = {}
    for name, value in headers.items():
        low = name.lower()
        if SECRET_HEADER.search(low):
            out[low] = NAME_ONLY
        else:
            out[low] = "<redacted: key pattern>" if find_secrets(value) else value
    return out


# ── SSE: framing and reassembly ────────────────────────────────────────────────


class SseFramer:
    """Incremental SSE framer. feed() returns whatever (event, data) frames are
    complete in the text so far, whatever the chunk boundaries look like;
    finish() flushes the tail of a stream that ended inside a frame."""

    def __init__(self):
        self._buf = ""
        self._event = None
        self._data = []

    def feed(self, text):
        frames = []
        self._buf += text
        while True:
            cut = self._buf.find("\n")
            if cut < 0:
                break
            self._line(self._buf[:cut], frames)
            self._buf = self._buf[cut + 1:]
        return frames

    def finish(self):
        frames = []
        if self._buf:
            self._line(self._buf, frames)
            self._buf = ""
        self._line("", frames)
        return frames

    def _line(self, line, frames):
        line = line.rstrip("\r")
        if not line:
            if self._data or self._event is not None:
                frames.append((self._event, "\n".join(self._data)))
            self._event, self._data = None, []
            return
        if line.startswith(":"):
            return                                  # comment / keep-alive
        field, _, value = line.partition(":")
        if value.startswith(" "):
            value = value[1:]
        if field == "event":
            self._event = value
        elif field == "data":
            self._data.append(value)


def parse_sse(text):
    framer = SseFramer()
    return framer.feed(text) + framer.finish()


def sse_text(events):
    """[(event, data)] back on the wire (tests, chunked replay)."""
    return "".join("event: %s\ndata: %s\n\n" % (event, json.dumps(data, ensure_ascii=False))
                   for event, data in events)


def _close_slot(slot):
    kind = slot["type"]
    if kind == "thinking":
        return {"type": "thinking", "thinking": slot["text"], "signature": slot["signature"]}
    if kind == "redacted_thinking":
        return {"type": "redacted_thinking", "data": slot["data"]}
    if kind == "tool_use":
        text = "".join(slot["parts"])
        try:
            parsed = json.loads(text) if text.strip() else {}
        except ValueError:
            parsed = {"_raw": text}
        return {"type": "tool_use", "id": slot["id"], "name": slot["name"], "input": parsed}
    if kind == "text":
        return {"type": "text", "text": slot["text"]}
    return dict(slot["start"])


def reassemble(frames):
    """Stream frames -> what the endpoint sent.

    Returns {"blocks", "stop_reason", "usage", "complete", "error"}. Blocks
    are in wire shape and arrival order; a tool_use input is the re-joined
    partial JSON (unparsable -> {"_raw": …}); usage takes the max reported per
    input/cache axis and the last reported output_tokens — the reading the
    kernel's reader applies too, so the usage check compares like with like.
    """
    slots, live = [], {}
    out = {"blocks": [], "stop_reason": None, "usage": {}, "complete": False, "error": None}

    def take(reported, final):
        if not isinstance(reported, dict):
            return
        for axis in AXES:
            value = reported.get(axis)
            if isinstance(value, bool) or not isinstance(value, (int, float)):
                continue
            if axis == "output_tokens" and final:
                out["usage"][axis] = int(value)
            else:
                out["usage"][axis] = max(int(out["usage"].get(axis, 0)), int(value))

    for event, data in frames:
        # `data` is the raw JSON text when the frames come off the wire (SseFramer)
        # and the parsed object when they come from a scripted reply.
        if isinstance(data, dict):
            payload = data
        else:
            try:
                payload = json.loads(data)
            except (TypeError, ValueError):
                continue
        if not isinstance(payload, dict):
            continue
        kind = payload.get("type") or event
        if kind == "message_start":
            take((payload.get("message") or {}).get("usage"), False)
        elif kind == "content_block_start":
            block = payload.get("content_block") or {}
            slot = {"type": block.get("type"), "text": "", "signature": "", "data": None,
                    "id": "", "name": "", "parts": [], "start": block}
            if slot["type"] == "thinking":
                slot["text"] = _str(block.get("thinking"))
                slot["signature"] = _str(block.get("signature"))
            elif slot["type"] == "redacted_thinking":
                slot["data"] = block.get("data") if isinstance(block.get("data"), str) else None
            elif slot["type"] == "text":
                slot["text"] = _str(block.get("text"))
            elif slot["type"] == "tool_use":
                slot["id"] = _str(block.get("id"))
                slot["name"] = _str(block.get("name"))
                seed = block.get("input")
                if isinstance(seed, dict) and seed:
                    slot["parts"].append(json.dumps(seed))
            slots.append(slot)
            live[payload.get("index", len(slots) - 1)] = slot
        elif kind == "content_block_delta":
            slot = live.get(payload.get("index"))
            if slot is None:
                continue
            delta = payload.get("delta") or {}
            piece = delta.get("type")
            if piece == "text_delta":
                slot["text"] += _str(delta.get("text"))
            elif piece == "thinking_delta":
                slot["text"] += _str(delta.get("thinking"))
            elif piece == "signature_delta":
                slot["signature"] += _str(delta.get("signature"))
            elif piece == "input_json_delta":
                slot["parts"].append(_str(delta.get("partial_json")))
        elif kind == "content_block_stop":
            live.pop(payload.get("index"), None)
        elif kind == "message_delta":
            reason = (payload.get("delta") or {}).get("stop_reason")
            if isinstance(reason, str):
                out["stop_reason"] = reason
            take(payload.get("usage"), True)
        elif kind == "message_stop":
            out["complete"] = True
        elif kind == "error":
            error = payload.get("error")
            out["error"] = (error.get("message") if isinstance(error, dict) else str(error)) or "stream error"
    out["blocks"] = [_close_slot(slot) for slot in slots]
    return out


# ── shapes and comparison ─────────────────────────────────────────────────────


def wire_content(message):
    """A message's content as wire-shape blocks ({"type": …}), from either the
    wire shape itself or the session file's externally tagged shape
    ({"text": …} / {"tool_use": {…}} / {"tool_result": {…}}). Prompt-caching
    markers are dropped: they are not part of what is compared."""
    content = message.get("content")
    if isinstance(content, str):
        return [{"type": "text", "text": content}]
    out = []
    for block in content or []:
        if not isinstance(block, dict):
            continue
        if "type" in block:
            out.append({k: v for k, v in block.items() if k != "cache_control"})
        elif "text" in block:
            out.append({"type": "text", "text": block["text"]})
        elif "tool_use" in block:
            call = block["tool_use"] or {}
            out.append({"type": "tool_use", "id": call.get("id"), "name": call.get("name"),
                        "input": call.get("input")})
        elif "tool_result" in block:
            result = block["tool_result"] or {}
            out.append({"type": "tool_result", "tool_use_id": result.get("tool_use_id"),
                        "is_error": bool(result.get("is_error"))})
        else:
            out.append({"type": next(iter(block), "unknown")})
    return out


def split_reasoning(blocks):
    """(reasoning, rest): reasoning as [(position, block)], position = number of
    non-reasoning blocks before it; rest = the other blocks in order."""
    reasoning, rest = [], []
    for block in blocks:
        if block.get("type") in REASONING_TYPES:
            reasoning.append((len(rest), block))
        else:
            rest.append(block)
    return reasoning, rest


def splice(reasoning, content):
    """The inverse of split_reasoning: wire content with the reasoning blocks
    back where their position says."""
    out, pending = [], [(pos, dict(block)) for pos, block in reasoning]
    for index, block in enumerate(content):
        while pending and pending[0][0] <= index:
            out.append(pending.pop(0)[1])
        out.append(block)
    out.extend(block for _, block in pending)
    return out


def replayable(block):
    """Whether the kernel may echo the block: a thinking block needs a
    signature, a redacted one its data. An unsigned thinking block is dropped
    on purpose and must never come back."""
    if block.get("type") == "thinking":
        return bool(block.get("signature"))
    return isinstance(block.get("data"), str)


def describe_message(message):
    counts = {}
    for block in wire_content(message):
        kind = block.get("type") or "?"
        counts[kind] = counts.get(kind, 0) + 1
    inner = " ".join("%s×%d" % (kind, count) for kind, count in counts.items()) or "empty"
    return "%s[%s]" % (message.get("role"), inner)


def _content_key(content):
    return tuple((block.get("type"), block.get("text")) for block in content)


def fp(text):
    """Short fingerprint of a signature / data string (never the value)."""
    if not text:
        return "none"
    return hashlib.sha256(text.encode("utf-8", "surrogatepass")).hexdigest()[:8]


def _short(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True)[:160]


def json_equal(a, b):
    """Equality of two JSON values the way the wire round trip preserves it:
    object key order is irrelevant, `1` and `1.0` are the same number, but
    `true` is not `1` and `1` is not `"1"`."""
    def norm(value):
        if isinstance(value, bool):
            return ("b", value)
        if isinstance(value, (int, float)):
            return ("n", float(value))
        if isinstance(value, dict):
            return ("o", sorted((str(key), norm(item)) for key, item in value.items()))
        if isinstance(value, list):
            return ("l", [norm(item) for item in value])
        return ("z", None) if value is None else ("s", value)
    return norm(a) == norm(b)


def _text_diff(a, b):
    if a == b:
        return None
    index = 0
    while index < min(len(a), len(b)) and a[index] == b[index]:
        index += 1
    return "text differs at char %d: the endpoint sent %d chars (%r…), got %d chars (%r…)" % (
        index, len(a), a[index:index + 24], len(b), b[index:index + 24])


def _rb(block):
    kind = block.get("type")
    if kind == "thinking":
        return "thinking(sig %s, %d chars)" % (fp(block.get("signature")), len(block.get("thinking") or ""))
    if kind == "redacted_thinking":
        return "redacted_thinking(data %s)" % fp(block.get("data"))
    return str(kind)


def compare_reasoning(expected, actual, where):
    """[(position, block)] against [(position, block)]: [] when every block is
    re-sent byte-identical (text, signature, position), else one line per
    difference. Parsed JSON values are compared, so key order is irrelevant."""
    problems = []
    if len(expected) != len(actual):
        problems.append("%s: expected %d reasoning block(s) [%s], got %d [%s]" % (
            where, len(expected), ", ".join(_rb(b) for _, b in expected) or "none",
            len(actual), ", ".join(_rb(b) for _, b in actual) or "none"))
    for index, ((epos, eb), (apos, ab)) in enumerate(zip(expected, actual)):
        label = "%s: reasoning block #%d (%s)" % (where, index + 1, _rb(eb))
        kind = eb.get("type")
        if ab.get("type") != kind:
            problems.append("%s: re-sent as %s" % (label, ab.get("type")))
            continue
        if epos != apos:
            problems.append("%s: re-sent before content block %d, the endpoint emitted it before %d" % (
                label, apos, epos))
        if kind == "thinking":
            diff = _text_diff(eb.get("thinking") or "", ab.get("thinking") or "")
            if diff:
                problems.append("%s: %s" % (label, diff))
            if (eb.get("signature") or "") != (ab.get("signature") or ""):
                problems.append("%s: signature differs (re-sent sig %s)" % (label, fp(ab.get("signature"))))
        elif kind == "redacted_thinking":
            if eb.get("data") != ab.get("data"):
                problems.append("%s: data differs (re-sent data %s)" % (label, fp(ab.get("data"))))
    return problems


def compare_content(expected, actual, where):
    """Reply content blocks against what a request re-sent: [(kind, message)]
    with kind "content", "text" or "tool" — [] when identical. Tool inputs are
    compared as parsed JSON (key order and 1 vs 1.0 do not count)."""
    problems = []
    if len(expected) != len(actual):
        problems.append(("content", "%s: expected %d content block(s) [%s], got %d [%s]" % (
            where, len(expected), ", ".join(b.get("type") or "?" for b in expected) or "none",
            len(actual), ", ".join(b.get("type") or "?" for b in actual) or "none")))
    for index, (eb, ab) in enumerate(zip(expected, actual)):
        kind = eb.get("type")
        label = "%s: content block #%d (%s)" % (where, index + 1, kind)
        if ab.get("type") != kind:
            problems.append(("content", "%s: re-sent as %s" % (label, ab.get("type"))))
        elif kind == "text":
            diff = _text_diff(eb.get("text") or "", ab.get("text") or "")
            if diff:
                problems.append(("text", "%s: %s" % (label, diff)))
        elif kind == "tool_use":
            if eb.get("id") != ab.get("id"):
                problems.append(("tool", "%s %s: re-sent with id %r" % (label, eb.get("id"), ab.get("id"))))
            if eb.get("name") != ab.get("name"):
                problems.append(("tool", "%s %s: re-sent as %r" % (label, eb.get("id"), ab.get("name"))))
            if not json_equal(eb.get("input"), ab.get("input")):
                problems.append(("tool", "%s %s: input changed — the endpoint sent %s, re-sent %s" % (
                    label, eb.get("id"), _short(eb.get("input")), _short(ab.get("input")))))
    return problems


class Reply:
    """One response the endpoint sent, in wire shape."""

    def __init__(self, blocks, stop_reason=None, usage=None, label=""):
        self.blocks = [dict(block) for block in blocks]
        has_call = any(block.get("type") == "tool_use" for block in self.blocks)
        self.stop_reason = stop_reason or ("tool_use" if has_call else "end_turn")
        self.usage = dict(usage or {})
        self.label = str(label)
        self.stored = True          # complete and error-free: a history candidate
        self.error = None
        reasoning, content = split_reasoning(self.blocks)
        # An unsigned thinking block is dropped by the kernel on purpose.
        self.reasoning = [(pos, block) for pos, block in reasoning if replayable(block)]
        # Empty text blocks never enter the history.
        self.content = [block for block in content
                        if block.get("type") != "text" or block.get("text")]
        self.order = ",".join(block.get("type") or "?" for block in self.blocks)

    @property
    def key(self):
        """Fast identity for a text-only reply (tool-call replies go by id)."""
        return _content_key(self.content)

    @classmethod
    def from_stream(cls, frames, label=""):
        sent = reassemble(frames)
        reply = cls(sent["blocks"], sent["stop_reason"], sent["usage"], label)
        reply.stored = bool(sent["complete"]) and not sent["error"]
        reply.error = sent["error"]
        return reply


# ── rules: shape, pairing, echo, lifetime, fold ───────────────────────────────


def is_summary_request(body):
    """The kernel's compression summary request is recognisable on the wire:
    no tools, the system prompt as a bare string (main-loop requests send it
    as a cached block list) and exactly one user message."""
    messages = body.get("messages")
    return (not body.get("tools") and not isinstance(body.get("system"), list)
            and isinstance(messages, list) and len(messages) == 1
            and isinstance(messages[0], dict) and messages[0].get("role") == "user")


def is_turn_opener(message):
    """A user message that opens a turn. The tool results of a running loop
    ride their own user message — those do not open a turn."""
    return message.get("role") == "user" and not any(
        block.get("type") == "tool_result" for block in wire_content(message))


def turn_indices(messages):
    """For each message, the number of the turn it belongs to (1-based)."""
    out, turn = [], 0
    for message in messages:
        if is_turn_opener(message):
            turn += 1
        out.append(turn)
    return out


def request_problems(body, where):
    """Shape rules every main-loop request must satisfy, as [(rule, message)]:
    roles alternate from a user message, the first block of the first message
    is text, tool_use ids are unique and answered N:N by the user message right
    after, and reasoning blocks appear only inside the assistant messages of
    the running turn — signed, with exactly the keys the API accepts."""
    problems = []
    messages = body.get("messages")
    if not isinstance(messages, list) or not messages:
        return [("shape", "%s: `messages` is missing or empty" % where)]
    for i, message in enumerate(messages):
        role = message.get("role") if isinstance(message, dict) else None
        if role != ("user" if i % 2 == 0 else "assistant"):
            problems.append(("shape", "%s: message %d has role %r; roles must alternate user/assistant "
                                     "starting from a user message" % (where, i, role)))
            return problems                 # the rules below need the cadence
    turns = turn_indices(messages)
    blocks = [wire_content(message) for message in messages]
    if not blocks[0] or blocks[0][0].get("type") != "text":
        first = blocks[0][0].get("type") if blocks[0] else "nothing"
        problems.append(("shape", "%s: the first message must open with a text block, got %s" % (where, first)))
    seen = set()
    for i, (message, msg_blocks) in enumerate(zip(messages, blocks)):
        role = message.get("role")
        if isinstance(message.get("content"), str):
            problems.append(("shape", "%s: message %d content is a bare string; the provider sends content arrays"
                                      % (where, i)))
        for block in msg_blocks:
            kind = block.get("type")
            if kind in REASONING_TYPES:
                if role != "assistant":
                    problems.append(("shape", "%s: message %d (user) carries a %s block" % (where, i, kind)))
                elif turns[i] != turns[-1]:
                    # The carry lasts one turn: a block of an earlier turn — that
                    # turn's final answer or its last tool call — ended when the
                    # next user message opened. (Inside the running turn a
                    # thinking block may sit on a tool-less assistant message,
                    # e.g. after a wrap-up nudge.)
                    problems.append(("lifetime", "%s: assistant message %d carries a %s block although its turn is "
                                     "over (turn %d, re-sent in turn %d)"
                                     % (where, i, kind, turns[i], turns[-1])))
                allowed = THINKING_KEYS if kind == "thinking" else REDACTED_KEYS
                extra = sorted(set(block) - allowed)
                if extra:
                    problems.append(("shape", "%s: message %d %s block carries key(s) %s the API rejects"
                                             % (where, i, kind, extra)))
                sealed = block.get("signature") if kind == "thinking" else block.get("data")
                if not (isinstance(sealed, str) and sealed):
                    what = "signature" if kind == "thinking" else "data"
                    problems.append(("shape", "%s: message %d %s block has an empty %s (the API rejects it)"
                                             % (where, i, kind, what)))
            elif kind == "text":
                if not (block.get("text") or ""):
                    problems.append(("shape", "%s: message %d has an empty text block" % (where, i)))
            elif kind == "tool_use":
                if role != "assistant":
                    problems.append(("shape", "%s: message %d (user) carries a tool_use block" % (where, i)))
                if block.get("id") in seen:
                    problems.append(("pairing", "%s: tool_use id %r is used more than once" % (where, block.get("id"))))
                seen.add(block.get("id"))
        if role == "user":
            answered = [block.get("tool_use_id") for block in msg_blocks if block.get("type") == "tool_result"]
            if answered:
                asked = [block.get("id") for block in blocks[i - 1] if block.get("type") == "tool_use"] if i else []
                if sorted(map(str, answered)) != sorted(map(str, asked)):
                    problems.append(("pairing", "%s: message %d answers tool_use %s but the assistant message before it "
                                                 "asked %s" % (where, i, answered, asked)))
    return problems


def summary_request_problems(body, where):
    """The summarizer sees the folded history as quoted text and nothing else."""
    problems = []
    for message in body.get("messages") or []:
        for block in wire_content(message):
            if block.get("type") in REASONING_TYPES:
                problems.append("%s: the summary request carries a %s block — the summarizer never sees carried reasoning"
                                % (where, block.get("type")))
    if "thinking" in body or "output_config" in body:
        problems.append("%s: the summary request carries thinking=%r / output_config=%r — the summary call is made "
                        "with thinking off" % (where, body.get("thinking"), body.get("output_config")))
    return problems


class Exchange:
    """One request the endpoint answered (or failed to), as seen from the wire."""

    def __init__(self, number, body, events=None, note=None, where=None, status=200):
        self.number = number
        self.body = body if isinstance(body, dict) else {}
        self.note = note
        self.where = where or "request #%d" % number
        self.status = status
        self.label = ""
        self.kind = ("other" if not isinstance(body, dict) or not isinstance(body.get("messages"), list)
                     else "summary" if is_summary_request(self.body) else "main")
        if events is not None and status == 200:
            self.reply = Reply.from_stream(events, str(number))
        else:
            self.reply = None


def message_key(content):
    """Identity of an assistant message on the wire: its first tool_use id, or
    the types and texts of its blocks when it carries no tool call."""
    for block in content:
        if block.get("type") == "tool_use":
            return ("tool", block.get("id"))
    return ("text", _content_key(content))


def _find_reply(replies, content, cursor):
    """Index of the first not-yet-consumed reply that `content` re-sends, or
    None. A tool call names its reply by id (the content is then compared
    against that reply by the caller, so a changed tool input is reported as
    what it is); a text-only reply by its own content. Replies before `cursor`
    are considered only when nothing from it on matches: the wire is not
    always one conversation — a fold, or a second run in the same capture,
    starts its history over."""
    key = message_key(content)
    if key[0] == "tool":
        candidates = [index for index, reply in enumerate(replies)
                      if any(block.get("id") == key[1] for block in reply.content)]
    else:
        candidates = [index for index, reply in enumerate(replies) if reply.key == key[1]]
    candidates.sort(key=lambda index: index < cursor)        # stable: from `cursor` on, then the rest
    if key[0] == "tool":
        return candidates[0] if candidates else None
    for index in candidates:
        if not compare_content(replies[index].content, content, "x"):
            return index
    return None


def echo_problems(body, replies, carry_on, strict_sources, where, forms=None):
    """Every assistant message of a request must be a reply the endpoint sent,
    re-sent verbatim, with its reasoning blocks echoed byte-identical inside
    their own turn.

    Returns ([(rule, message)], {reply index: (reasoning, rest, echo due)}, counts).
    Rules: echo (a block that should be re-sent is not byte-identical — or one
    of an earlier turn, carried anyway, is), carry (blocks re-sent although
    carry is off or thinking is disabled), content / tool (text or tool input
    changed), source (the mock never sent this message), unverified (a capture
    that started mid-session has no reply to compare against — the message is
    then only held to its own earlier form, kept in `forms`). Where a block may
    appear at all (the lifetime rule) is request_problems'.
    """
    problems, matched = [], {}
    counts = {"echoed": 0, "expected": 0}
    messages = body.get("messages") or []
    turns = turn_indices(messages)
    disabled = isinstance(body.get("thinking"), dict) and body["thinking"].get("type") == "disabled"
    cursor = 0
    for i, message in enumerate(messages):
        if not isinstance(message, dict) or message.get("role") != "assistant":
            continue
        reasoning, rest = split_reasoning(wire_content(message))
        index = _find_reply(replies, rest, cursor)
        if index is None:
            key = message_key(rest)
            if forms is not None:
                previous = forms.get(key)
                if previous is not None:
                    earlier, was = previous
                    changed = list(compare_reasoning(earlier[0], reasoning, "%s: assistant message %d" % (where, i)))
                    changed += [text for _, text in compare_content(earlier[1], rest, "%s: assistant message %d" % (where, i))]
                    if changed:
                        problems.extend(("echo", "%s — not re-sent as it was in %s" % (text, was)) for text in changed)
                else:
                    forms[key] = ((reasoning, rest), where)
            if reasoning or strict_sources:
                rule = "source" if strict_sources else "unverified"
                lead = "carries %d reasoning block(s) but " % len(reasoning) if reasoning else ""
                problems.append((rule, "%s: assistant message %d %smatches no reply the endpoint sent [%s]" % (
                    where, i, lead, ", ".join(block.get("type") or "?" for block in rest) or "no content blocks")))
            continue
        cursor = index + 1
        reply = replies[index]
        live = carry_on and not disabled
        earlier = turns[i] != turns[-1]
        expected = reply.reasoning if live and not earlier else []
        against = "%s: assistant message %d (reply #%s)" % (where, i, reply.label)
        if live and earlier and reasoning:
            # Not due (rule lifetime belongs to request_problems) but carried
            # anyway: then still the endpoint's bytes.
            problems.extend(("echo", text) for text in compare_reasoning(reply.reasoning, reasoning, against))
        else:
            problems.extend(("echo" if live else "carry", problem)
                            for problem in compare_reasoning(expected, reasoning, against))
        counts["echoed"] += len(reasoning)
        counts["expected"] += len(expected)
        for kind, problem in compare_content(reply.content, rest,
                                             "%s: assistant message %d (reply #%s)" % (where, i, reply.label)):
            problems.append(("tool" if kind == "tool" else "content", problem))
        # (what the wire re-sent, whether an echo was due at all)
        matched[index] = (reasoning, rest, bool(expected))
    return problems, matched, counts


def _same_message(a, b):
    """Two messages with the same role, the same reasoning blocks and the same
    content (a tool_result by its id: its content may be pruned)."""
    if a.get("role") != b.get("role"):
        return False
    a_reasoning, a_rest = split_reasoning(wire_content(a))
    b_reasoning, b_rest = split_reasoning(wire_content(b))
    if compare_reasoning(a_reasoning, b_reasoning, "x"):
        return False
    if a.get("role") == "assistant":
        return not compare_content(a_rest, b_rest, "x")
    shape = lambda blocks: [(block.get("type"), block.get("tool_use_id")) for block in blocks
                            if block.get("type") == "tool_result"]
    texts = lambda blocks: [block for block in blocks if block.get("type") != "tool_result"]
    return shape(a_rest) == shape(b_rest) and not compare_content(texts(a_rest), texts(b_rest), "x")


def fold_history(exchange):
    """The history as it stood when a fold ran right after `exchange`: the
    request's messages, then the reply they got, then the user message that
    answers its tool calls (their outputs are not on the wire before the fold —
    they are matched by id)."""
    reply = exchange.reply
    messages = [dict(message) for message in exchange.body.get("messages") or []]
    if reply is None or not reply.stored:
        return messages
    blocks = [{k: v for k, v in block.items() if k != "cache_control"}
              for block in splice(reply.reasoning, reply.content)]
    messages.append({"role": "assistant", "content": blocks})
    results = [{"type": "tool_result", "tool_use_id": block.get("id"), "content": "<tool output>"}
               for block in reply.content if block.get("type") == "tool_use"]
    if results:
        messages.append({"role": "user", "content": results})
    return messages


def fold_problems(history, post, summary, where):
    """What a compression fold must look like on the wire. `history` is the
    conversation the summariser saw (fold_history); `post` the first main-loop
    request after the summary request.

    Rules: post = [one user message whose last text block carries the summary]
    + an untouched assistant-led tail of `history`; nothing else survives.
    Returns (problems, {"before", "after", "folded", "kept"}) — folded / kept
    are None when the tail was not recognised."""
    post_messages = post.get("messages") or []
    problems = []
    summary_text = " ".join(block.get("text") or "" for block in summary.content
                            if block.get("type") == "text").strip()
    head = wire_content(post_messages[0]) if post_messages else []
    texts = [block.get("text") or "" for block in head if block.get("type") == "text"]
    if (not summary_text or not texts or summary_text not in texts[-1]
            or any(block.get("type") not in ("text", "tool_result") for block in head)
            or post_messages[0].get("role") != "user"):
        problems.append("%s: the first message after the summary request is not the summary head (a user message "
                        "whose last text block carries the summary text)" % where)
    tail = post_messages[1:]
    kept = not tail
    for start, message in enumerate(history):
        if (message.get("role") == "assistant" and len(history) - start == len(tail)
                and all(_same_message(a, b) for a, b in zip(history[start:], tail))):
            kept = True
            break
    # The first message stays as the head of the folded history unless the
    # conversation itself opened with an assistant message (fresh empty head).
    head = 1 if history and history[0].get("role") == "user" else 0
    facts = {"before": len(history), "after": len(post_messages),
             "folded": len(history) - len(tail) - head if kept else None, "kept": len(tail) if kept else None}
    if not kept:
        problems.append("%s: after the fold the history is not [summary head] + an untouched assistant-led tail of "
                        "the conversation the summariser saw (%d message(s) after, %d before)"
                        % (where, len(post_messages), len(history)))
    return problems, facts


def describe_exchange(exchange):
    body = exchange.body
    messages = body.get("messages") or []
    line = "#%d %s, %d message(s): %s" % (
        exchange.number, exchange.kind, len(messages),
        " ".join(describe_message(message) for message in messages if isinstance(message, dict)))
    for key in ("thinking", "output_config"):
        if key in body:
            line += "  %s=%s" % (key, json.dumps(body[key]))
    reply = exchange.reply
    if reply is None:
        line += "\n      -> no reply (%s)" % (exchange.note or "nothing recorded")
    else:
        line += "\n      -> [%s] stop=%s usage %s" % (
            reply.order, reply.stop_reason,
            " ".join("%s=%s" % (axis, reply.usage.get(axis, 0)) for axis in AXES))
    return line


def audit_traffic(exchanges, carry_on, strict_sources=False, detail=None):
    """Every rule over one conversation's exchanges, in arrival order.

    carry_on: whether the run had `preserved_reasoning` on. With
    strict_sources (the mock saw everything) an assistant message that matches
    no recorded reply is a problem; without it (a capture that may have started
    mid-session) it is an unverifiable note and the message is held to its own
    earlier form instead. Returns
    {rules, notes, replies, last_wire, wire_forms, stats, folds}.
    """
    rules, notes = {}, {}

    def add(rule, message):
        rules.setdefault(rule, []).append(message)

    replies = []
    for exchange in exchanges:
        reply = exchange.reply
        if exchange.kind == "main" and reply is not None and reply.stored:
            replies.append(reply)
    stats = {"main": 0, "summary": 0, "other": 0, "echo_requests": 0, "echoed": 0, "expected": 0,
             "calls": 0, "orders": [], "turns": 1, "unusable": 0}
    last_wire, folds, pending, last_main = {}, [], None, None
    forms = None if strict_sources else {}
    for exchange in exchanges:
        if detail is not None:
            detail(describe_exchange(exchange))
        reply = exchange.reply
        usable = reply is not None and reply.stored
        if reply is None:
            add("response", "%s: no usable response was recorded (%s)" % (exchange.where, exchange.note or "nothing captured"))
        elif not reply.stored:
            add("response", "%s: the response is incomplete%s" % (
                exchange.where, " — stream error: %s" % reply.error if reply.error else " (it ended before message_stop)"))
        if exchange.kind != "main":
            if not usable:
                continue
            if exchange.kind == "summary":
                stats["summary"] += 1
                for problem in summary_request_problems(exchange.body, exchange.where):
                    add("summary", problem)
                pending = (exchange, last_main, stats["main"])
            else:
                stats["other"] += 1
            continue
        stats["main"] += 1
        messages = exchange.body.get("messages") or []
        stats["turns"] = max(stats["turns"], turn_indices(messages)[-1] if messages else 1)
        # The shape of a request is a fact about the request alone: it is
        # checked whether or not the response can be used.
        for rule, problem in request_problems(exchange.body, exchange.where):
            add(rule, problem)
        if not usable:
            stats["unusable"] += 1
            continue
        if reply.order not in stats["orders"]:
            stats["orders"].append(reply.order)
        stats["calls"] += sum(1 for block in reply.content if block.get("type") == "tool_use")
        found, matched, counts = echo_problems(exchange.body, replies, carry_on,
                                               strict_sources, exchange.where, forms)
        for rule, problem in found:
            if rule == "unverified":
                notes.setdefault(rule, []).append(problem)
            else:
                add(rule, problem)
        last_wire.update(matched)
        stats["echo_requests"] += 1 if counts["expected"] else 0
        stats["echoed"] += counts["echoed"]
        stats["expected"] += counts["expected"]
        if pending is not None:
            summary, before, count = pending
            if before is not None:
                found, facts = fold_problems(fold_history(before), exchange.body, summary.reply, exchange.where)
                for problem in found:
                    add("summary", problem)
                folds.append({"summary": summary, "pre": before, "post": exchange, "before": count,
                              "after_index": stats["main"], "facts": facts})
            pending = None
        last_main = exchange
    if exchanges and not stats["main"]:
        add("shape", "no main-loop request was recognised — with no tool configured every request has the shape of "
                     "a summary request; give the run at least one tool")
    return {"rules": rules, "notes": notes, "replies": replies,
            "last_wire": last_wire, "wire_forms": forms, "stats": stats, "folds": folds}


# ── usage math ────────────────────────────────────────────────────────────────


def sum_usage(exchanges):
    total = dict.fromkeys(AXES, 0)
    for exchange in exchanges:
        for axis in AXES:
            value = exchange.reply.usage.get(axis) if exchange.reply is not None else None
            if isinstance(value, (int, float)) and not isinstance(value, bool):
                total[axis] += int(value)
    return total


def usage_problems(result, exchanges, where, expect_turns=True):
    """result.json's usage must equal what the endpoint reported, summed over
    every request of the run — the main loop and the summary call alike; and
    `turns` must be the number of main-loop requests."""
    total = sum_usage(exchanges)
    problems = []
    for axis in AXES:
        if result.get(axis) != total[axis]:
            problems.append("%s: %s is %s in result.json, the endpoint reported %s over %d request(s)"
                            % (where, axis, result.get(axis), total[axis], len(exchanges)))
    if "reasoning_tokens" in result:
        problems.append("%s: result.json carries reasoning_tokens=%s; the Anthropic protocol reports no such split"
                        % (where, result["reasoning_tokens"]))
    main = sum(1 for exchange in exchanges if exchange.kind == "main")
    if expect_turns and result.get("turns") != main:
        problems.append("%s: turns is %s in result.json, the wire saw %d main-loop request(s)"
                        % (where, result.get("turns"), main))
    return problems


# ── the session file (three-way) ──────────────────────────────────────────────


def session_reasoning(message, where):
    """`Message.reasoning` (the session file's side field) as [(before, wire block)]."""
    field = message.get("reasoning")
    if field is None:
        return [], []
    blocks, problems = [], []
    if field.get("provider") != "anthropic":
        problems.append("%s: reasoning.provider is %r, not 'anthropic'" % (where, field.get("provider")))
    for item in field.get("blocks") or []:
        before = item.get("before")
        if not isinstance(before, int) or isinstance(before, bool):
            problems.append("%s: reasoning block without an integer `before`" % where)
            before = -1
        opaque = item.get("opaque") or {}
        if isinstance(opaque.get("signature"), str) and opaque["signature"]:
            block = {"type": "thinking", "thinking": item.get("text") or "", "signature": opaque["signature"]}
        elif isinstance(opaque.get("data"), str):
            block = {"type": "redacted_thinking", "data": opaque["data"]}
        else:
            problems.append("%s: reasoning block carries neither a signature nor redacted data" % where)
            continue
        blocks.append((before, block))
    return blocks, problems


def session_problems(session, audit, carry_on, where="session", strict_sources=False, skip=()):
    """Three-way over the session file: endpoint reply ↔ `Message.reasoning`
    side field ↔ the last wire echo. Returns ([(rule, message)], stats)."""
    rules, stats = [], {"messages": 0, "blocks": 0, "echoed": 0, "silent": 0, "final": 0, "wire_only": 0, "archived": 0}
    replies, last_wire = audit["replies"], audit["last_wire"]
    forms = {} if strict_sources else (audit.get("wire_forms") or {})
    messages = session.get("messages") or []
    if not messages:
        return [("session", "%s: the session has no messages" % where)], stats
    turns = turn_indices(messages)
    cursor = 0
    for i, message in enumerate(messages):
        if not isinstance(message, dict):
            continue
        label = "message %d" % i
        if message.get("role") != "assistant":
            if "reasoning" in message:
                rules.append(("session", "%s: %s message %d carries `reasoning`" % (where, message.get("role"), i)))
            continue
        stats["messages"] += 1
        stored, errors = session_reasoning(message, "%s: %s" % (where, label))
        rules.extend(("session", error) for error in errors)
        stats["blocks"] += len(stored)
        embedded, rest = split_reasoning(wire_content(message))
        if embedded:
            rules.append(("session", "%s: %s carries %d reasoning block(s) inside its content — they belong in the "
                                  "`reasoning` side field alone" % (where, label, len(embedded))))
        live_turn = turns[i] == turns[-1]
        found = _find_reply(replies, rest, cursor)
        if found is None:
            form = forms.get(message_key(rest))
            if stored and form is not None and form[0][0]:
                # The endpoint's reply predates the capture, but the wire's own
                # echo is there: the side field is still held to it (two-way).
                against = "%s: %s vs its echo in %s" % (where, label, form[1])
                differences = list(compare_reasoning(stored, form[0][0], against))
                differences += [text for _, text in compare_content(rest, form[0][1], against)]
                rules.extend(("session", problem) for problem in differences)
                stats["wire_only"] += 1
            if stored and carry_on and not live_turn:
                rules.append(("lifetime", "%s: %s keeps %d reasoning block(s) of a turn that is over (turn %d of %d)"
                              % (where, label, len(stored), turns[i], turns[-1])))
            if stored or strict_sources:
                rule = "source" if strict_sources else "unverified"
                lead = "carries %d reasoning block(s) but " % len(stored) if stored else ""
                rules.append((rule, "%s: %s %smatches no reply the endpoint sent" % (where, label, lead)))
            continue
        cursor = found + 1
        reply = replies[found]
        against = "%s: %s (reply #%s)" % (where, label, reply.label)
        if carry_on and not live_turn:
            # An earlier turn: nothing is due. Blocks kept anyway are a filed
            # gap — and still held to the endpoint's bytes.
            if stored:
                rules.append(("lifetime", "%s: %s keeps %d reasoning block(s) of a turn that is over (turn %d of %d)"
                              % (where, label, len(stored), turns[i], turns[-1])))
                rules.extend(("session", problem) for problem in compare_reasoning(reply.reasoning, stored, against))
        else:
            # Stored blocks are the endpoint's (carry on) or there are none (carry off).
            rules.extend(("session", problem) for problem in
                         compare_reasoning(reply.reasoning if carry_on else [], stored, against))
        rules.extend(("session", problem) for _, problem in
                     compare_content(reply.content, rest, "%s: %s (reply #%s)" % (where, label, reply.label)))
        if found not in last_wire:
            stats["final"] += 1            # the final reply is never re-sent
            continue
        wire_reasoning, wire_rest, due = last_wire[found]
        against = "%s: %s vs the last request that re-sent reply #%s" % (where, label, reply.label)
        differences = []
        if due:
            stats["echoed"] += len(stored)
            differences += list(compare_reasoning(stored, wire_reasoning, against))
        else:
            # Stored, and the wire re-sent the message without it — by design
            # when thinking is disabled for the request; whether an echo was
            # due is the traffic audit's question (rules echo / carry / lifetime).
            stats["silent"] += len(stored)
        differences += [text for _, text in compare_content(rest, wire_rest, against)]
        rules.extend(("session", problem) for problem in differences)
    # The archive holds the folded originals: never their reasoning, content intact.
    cursor = 0
    for i, message in enumerate(session.get("archive") or []):
        if not isinstance(message, dict):
            continue
        if "reasoning" in message:
            rules.append(("session", "%s: archived message %d carries `reasoning` (the session archive never does)"
                                    % (where, i)))
        if message.get("role") != "assistant":
            continue
        stats["archived"] += 1
        rest = split_reasoning(wire_content(message))[1]
        found = _find_reply(replies, rest, cursor)
        if found is None:
            rules.append(("source" if strict_sources else "unverified",
                          "%s: archived message %d matches no reply the endpoint sent" % (where, i)))
            continue
        cursor = found + 1
        rules.extend(("session", problem) for _, problem in
                     compare_content(replies[found].content, rest,
                                     "%s: archived message %d (reply #%s)" % (where, i, replies[found].label)))
    return [item for item in rules if item[0] not in skip], stats


# ── scripted replies: the mock's vocabulary (shared with the self-test) ───────

CHUNK = 700            # bytes per HTTP chunk: boundaries ignore events and characters

THINKING = [
    'Plan: list the directory first, then write the notes — 步骤：① 列目录 ② 写文件\n'
    'path C:\\tmp\\notes ✓ 🚀\tsecond line, trailing spaces follow   ',
    '  Leading spaces, "double quotes", a backslash \\ and a slash / — keep every byte  \n',
    'Draft #3: check the result of the previous call before editing.\n\n(blank line above)',
    "It's the fourth plan: l'apostrophe, 中文，emoji 🧠 and <tags>\n\tindented",
    'Final pass: 回顾 — everything verified ✓\n',
]


def signature(label):
    """Deterministic per label, unique, and shaped like a real one: hundreds
    of base64 characters, so a truncated or re-wrapped signature shows up as a
    mismatch instead of passing unnoticed."""
    length = 420 + (len(label) * 131) % 700
    parts, counter = [], 0
    while sum(len(part) for part in parts) < length:
        digest = hashlib.sha256(("e2e-anthropic-wire#%s#%d" % (label, counter)).encode()).digest()
        parts.append(base64.b64encode(digest).decode())
        counter += 1
    return "".join(parts)[:length]


def thinking(content, sig):
    return {"type": "thinking", "thinking": content, "signature": sig}


def redacted(data):
    return {"type": "redacted_thinking", "data": data}


def text(content):
    return {"type": "text", "text": content}


def tool_use(name, args, call_id):
    return {"type": "tool_use", "id": call_id, "name": name, "input": args}


def pieces(value, count=3):
    """A string in `count` deltas, cut anywhere."""
    if not value:
        return []
    size = max(1, -(-len(value) // count))
    return [value[start:start + size] for start in range(0, len(value), size)]


def usage_for(number, body=None):
    """Usage the mock reports for request `number`: distinct, non-zero values
    on every axis, and prompt axes that grow with the request. The second part
    is not cosmetic — the kernel calibrates its context estimate on the
    provider's numbers, so a mock reporting a thousand tokens for a 300 KB
    request would keep a compression fold from ever tripping."""
    prompt = 900 if body is None else 700 + len(json.dumps(body, ensure_ascii=False)) // 4
    read = max(500 + 13 * number, prompt // 2)
    created = 25 + (number % 3) * 25
    return {"input_tokens": prompt - read - created + 37 * number, "output_tokens": 40 + 11 * number,
            "cache_creation_input_tokens": created, "cache_read_input_tokens": read}


def reply_events(reply, number):
    """A scripted reply as the SSE frames a real endpoint sends for it:
    message_start, one content block per block, message_delta, message_stop."""
    usage = reply.usage
    start = {"type": "content_block_start", "index": 0, "content_block": {}}
    delta = {"type": "content_block_delta", "index": 0, "delta": {}}
    events = [("message_start", {"type": "message_start", "message": {
        "id": "msg_%d" % number, "type": "message", "role": "assistant", "model": "mock-claude",
        "content": [], "stop_reason": None, "stop_sequence": None,
        "usage": {"input_tokens": usage.get("input_tokens", 0),
                  "cache_creation_input_tokens": usage.get("cache_creation_input_tokens", 0),
                  "cache_read_input_tokens": usage.get("cache_read_input_tokens", 0),
                  "output_tokens": 1}}}), ("ping", {"type": "ping"})]

    def frame(base, index, **fields):
        event = dict(base, index=index)
        event.update(fields)
        return (event["type"], event)

    for index, block in enumerate(reply.blocks):
        kind = block["type"]
        if kind == "thinking":
            events.append(frame(start, index, content_block={"type": "thinking", "thinking": "", "signature": ""}))
            for piece in pieces(block["thinking"]):
                events.append(frame(delta, index, delta={"type": "thinking_delta", "thinking": piece}))
            for piece in pieces(block["signature"], 2):
                events.append(frame(delta, index, delta={"type": "signature_delta", "signature": piece}))
        elif kind == "redacted_thinking":
            events.append(frame(start, index, content_block={"type": "redacted_thinking", "data": block["data"]}))
        elif kind == "text":
            events.append(frame(start, index, content_block={"type": "text", "text": ""}))
            for piece in pieces(block["text"]):
                events.append(frame(delta, index, delta={"type": "text_delta", "text": piece}))
        elif kind == "tool_use":
            events.append(frame(start, index, content_block={"type": "tool_use", "id": block["id"],
                                                             "name": block["name"], "input": {}}))
            for piece in pieces(json.dumps(block["input"], ensure_ascii=False)):
                events.append(frame(delta, index, delta={"type": "input_json_delta", "partial_json": piece}))
        events.append(("content_block_stop", {"type": "content_block_stop", "index": index}))
    events.append(("message_delta", {"type": "message_delta",
                                     "delta": {"stop_reason": reply.stop_reason, "stop_sequence": None},
                                     "usage": {"input_tokens": usage.get("input_tokens", 0),
                                               "output_tokens": usage.get("output_tokens", 0)}}))
    events.append(("message_stop", {"type": "message_stop"}))
    return events


# ── the mutation self-test ────────────────────────────────────────────────────


def self_test():
    """The checker checks itself. Every tampering must be caught by the same
    comparisons the audits use — on the wire (echo_problems) and in the session
    file (session_problems) — while the two harmless re-encodings must not be.
    Returns what went wrong ([] = the checker is sound)."""
    found = []
    sig = signature("self-test")
    scripted = Reply([thinking("Draft: write the file, then verify it.", sig), text("Writing it."),
                      tool_use("write_file", {"path": "a.txt", "content": "x\ny", "n": 1, "flag": True},
                               "toolu_self_1"),
                      tool_use("list_dir", {}, "toolu_self_2")], "tool_use", usage_for(1))
    opening = {"id": "m1", "role": "user", "content": [{"type": "text", "text": "go"}]}
    result = {"id": "m3", "role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_self_1", "content": "wrote a.txt (3 bytes)"},
        {"type": "tool_result", "tool_use_id": "toolu_self_2", "content": "a.txt"}]}
    tools = [{"name": "write_file", "description": "writes a file", "input_schema": {"type": "object"}},
             {"name": "list_dir", "description": "lists a directory", "input_schema": {"type": "object"}}]
    audit = audit_traffic([Exchange(1, {"messages": [opening], "tools": tools}, reply_events(scripted, 1),
                                     where="self-test")], True, True)
    sent = audit["replies"][0]
    other = "B" if sig[-1] != "B" else "C"

    def wire(mutate):
        blocks = mutate(splice(sent.reasoning, sent.content))
        body = {"messages": [opening, {"role": "assistant", "content": blocks}, result]}
        problems, _, _ = echo_problems(body, audit["replies"], True, True, "self-test")
        return [message for _, message in problems]

    def session(mutate):
        message = {
            "id": "m2", "role": "assistant",
            "content": [{"text": "Writing it."},
                        {"tool_use": {"id": "toolu_self_1", "name": "write_file",
                                      "input": {"path": "a.txt", "content": "x\ny", "n": 1, "flag": True}}},
                        {"tool_use": {"id": "toolu_self_2", "name": "list_dir", "input": {}}}],
            "reasoning": {"provider": "anthropic", "blocks": [
                {"before": 0, "text": "Draft: write the file, then verify it.", "opaque": {"signature": sig}}]},
        }
        mutate(message)
        return {"version": 1, "model": "mock-claude", "messages": [dict(opening), message]}

    def in_session(book):
        return [message for _, message in session_problems(book, audit, True, "self-test", True)[0]]

    def first_tool(blocks):
        return next(block for block in blocks if block.get("type") == "tool_use")

    def reorder(blocks):
        return [dict(reversed(list(block.items()))) for block in blocks]

    def float_one(blocks):
        blocks = [dict(block) for block in blocks]
        first_tool(blocks)["input"] = dict(first_tool(blocks)["input"], n=1.0)
        return blocks

    def s_alter_signature(message):
        message["reasoning"]["blocks"][0]["opaque"]["signature"] = other + sig[1:]

    def s_alter_text(message):
        message["reasoning"]["blocks"][0]["text"] += " "          # a trailing space is a different byte string

    def s_move(message):
        message["reasoning"]["blocks"][0]["before"] = 1

    def s_drop(message):
        del message["reasoning"]

    def s_tool_input(message):
        del message["content"][1]["tool_use"]["input"]["n"]

    def s_reorder(message):
        for position, block in enumerate(message["content"]):
            message["content"][position] = dict(reversed(list(block.items())))
        message["reasoning"]["blocks"][0]["opaque"] = dict(reversed(list(message["reasoning"]["blocks"][0]["opaque"].items())))

    def s_float_one(message):
        message["content"][1]["tool_use"]["input"]["n"] = 1.0

    def alter_signature(blocks):
        blocks = [dict(block) for block in blocks]
        blocks[0]["signature"] = other + sig[1:]
        return blocks

    def alter_text(blocks):
        blocks = [dict(block) for block in blocks]
        blocks[0]["thinking"] += " "
        return blocks

    def move(blocks):
        return [blocks[1], blocks[0]] + blocks[2:]

    def drop(blocks):
        return blocks[1:]

    def tool_input(blocks):
        blocks = [dict(block) for block in blocks]
        first_tool(blocks)["input"] = {key: value for key, value in first_tool(blocks)["input"].items() if key != "n"}
        return blocks

    faithful = wire(lambda blocks: blocks) + in_session(session(lambda message: message))
    if faithful:
        found.append("the faithful copy is flagged: %s" % faithful[0])
    for label, on_wire, in_file in (("signature altered", alter_signature, s_alter_signature),
                                    ("text altered", alter_text, s_alter_text),
                                    ("position moved", move, s_move),
                                    ("block dropped", drop, s_drop),
                                    ("tool input changed", tool_input, s_tool_input)):
        caught = wire(on_wire)
        if not caught:
            found.append("%s is not caught on the wire" % label)
        caught = in_session(session(in_file))
        if not caught:
            found.append("%s is not caught in the session file" % label)
    for label, on_wire, in_file in (("key order", reorder, s_reorder), ("1 vs 1.0", float_one, s_float_one)):
        flagged = wire(on_wire) + in_session(session(in_file))
        if flagged:
            found.append("%s is flagged although it is not a change: %s" % (label, flagged[0]))
    return found


SELF_TEST_OK = ("signature altered, text altered, position moved, block dropped, tool input changed — each caught on "
                "the wire and in the session file by the same comparison the audits use (10 tampered copies); key "
                "order and 1 vs 1.0 are not flagged")


# ── the scripted endpoint ─────────────────────────────────────────────────────


def protocol_problem(headers, body):
    """Why a request is not a well-formed Messages API call (None when it is)."""
    if not isinstance(body, dict):
        return "the request body is not a JSON object"
    if body.get("stream") is not True:
        return '"stream" is not true'
    if headers.get("x-api-key") is None:
        return "no x-api-key header"
    if headers.get("anthropic-version") != ANTHROPIC_VERSION:
        return "anthropic-version is %r" % headers.get("anthropic-version")
    if not isinstance(body.get("max_tokens"), int) or isinstance(body.get("max_tokens"), bool):
        return "max_tokens is missing"
    if not isinstance(body.get("model"), str) or not body["model"]:
        return "model is missing"
    return None


class Script:
    """What the mock answers. `replies` are consumed by main-loop requests in
    order; `dynamic(n, script)` replaces the list when the answer depends on
    what was asked so far (a fold that trips at an unknown round); `summary`
    is the reply to a summary request."""

    def __init__(self, replies=(), summary=None, dynamic=None):
        self.replies = list(replies)
        self.summary = summary
        self.dynamic = dynamic
        self.summaries = 0

    def main(self, n):
        if self.dynamic is not None:
            return self.dynamic(n, self)
        return self.replies[n - 1] if n <= len(self.replies) else None

    def on_summary(self):
        self.summaries += 1
        return self.summary


class Mock:
    """The scripted Messages API endpoint: one script at a time, every exchange
    recorded in memory."""

    def __init__(self):
        self.lock = threading.Lock()
        self.records = []
        self.tag = ""
        self.script = None
        self.main_count = 0
        self.seq = 0            # requests since begin(): usage is derived from it, per scenario

    def begin(self, tag, script):
        with self.lock:
            self.tag = tag
            self.script = script if isinstance(script, Script) else Script(script)
            self.main_count = 0
            self.seq = 0

    def _record(self, headers, body, kind, reply, error):
        with self.lock:
            self.seq += 1
            if reply is not None and not reply.usage:
                reply.usage = usage_for(self.seq, body)     # only the self-test leaves it unset
            record = {"n": len(self.records) + 1, "seq": self.seq, "tag": self.tag, "kind": kind, "at": utcnow(),
                      "path": "/v1/messages", "headers": safe_headers(headers), "body": body,
                      "error": error, "events": reply_events(reply, self.seq) if reply else None}
            self.records.append(record)
        return record

    def reject(self, headers, body, problem):
        return self._record(headers, body, "invalid", None, problem)

    def answer(self, headers, body):
        script = self.script
        if is_summary_request(body):
            reply = script.on_summary() if script is not None and script.summary is not None else None
            return self._record(headers, body, "summary", reply, None if reply else "no summary reply is scripted")
        with self.lock:
            self.main_count += 1
            count = self.main_count
        reply = script.main(count) if script is not None else None
        return self._record(headers, body, "main", reply,
                            None if reply else "main-loop request #%d has no scripted reply" % count)


class MockHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"       # keep-alive + chunked streams, like the real endpoint
    timeout = 30

    def log_message(self, *args):
        pass

    def do_POST(self):
        try:
            self._serve()
        except OSError:                 # the client went away mid-stream
            self.close_connection = True

    def _serve(self):
        if self.path.split("?", 1)[0] != "/v1/messages":
            self._send_error(404, "not_found_error", "unexpected path %r" % self.path.split("?", 1)[0])
            return
        try:
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
        except ValueError:
            body = None
        problem = protocol_problem(self.headers, body)
        mock = self.server.mock
        record = mock.reject(self.headers, body, problem) if problem else mock.answer(self.headers, body)
        if record["events"] is None:
            self._send_error(400, "invalid_request_error", "mock endpoint: %s" % record["error"])
            return
        self._stream(record["events"])

    def _stream(self, events):
        payload = sse_text(events).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        for start in range(0, len(payload), CHUNK):
            piece = payload[start:start + CHUNK]
            self.wfile.write(b"%x\r\n%s\r\n" % (len(piece), piece))
        self.wfile.write(b"0\r\n\r\n")

    def _send_error(self, status, kind, message):
        payload = json.dumps({"type": "error", "error": {"type": kind, "message": message}}).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


class LoopbackServer(ThreadingHTTPServer):
    """The base of both servers here (the mock endpoint, the capture proxy): a
    ThreadingHTTPServer bound WITHOUT the reverse lookup. HTTPServer's
    server_bind() runs socket.getfqdn() on the bound address — a blocking
    gethostbyaddr() that sits out the resolver's whole timeout on a machine
    whose reverse lookup is slow or wedged (seen: the gate hung at startup
    before printing a line). The name it produces feeds CGI environment fields
    only; nothing here reads it, and a loopback server has no use for one."""
    daemon_threads = True

    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        host, port = self.server_address[:2]
        self.server_name, self.server_port = host, port


class MockServer(LoopbackServer):
    def __init__(self, address, mock):
        self.mock = mock
        super().__init__(address, MockHandler)

    @property
    def base_url(self):
        return "http://127.0.0.1:%d" % self.server_address[1]


# ── check: driving the real binary through the mock ───────────────────────────


class Reporter:
    """Collects checks into [PASS]/[FAIL]/[GAP] lines and the exit status."""

    def __init__(self, verbose=False, only=None, strict=False):
        self.verbose, self.only, self.strict = verbose, only, strict or STRICT
        self.counts = {"PASS": 0, "FAIL": 0, "GAP": 0}
        self.failed = []
        self.gapped = set()          # rules filed as a known gap in this run

    def selected(self, name):
        return self.only is None or self.only in name

    def emit(self, name, problems, ok, gaps=(), always=False):
        """One check = one line: `problems` is empty when the check holds, else
        [(rule, message)] (a plain string counts as rule "check"). A check whose
        every problem belongs to a filed gap is [GAP], not [FAIL], unless strict.
        `always` prints the line even under --only."""
        if not self.selected(name) and not always:
            return True
        if isinstance(problems, str):
            problems = [problems]
        problems = [item if isinstance(item, tuple) else ("check", item) for item in (problems or [])]
        if not problems:
            status = "PASS"
        elif all(rule in gaps and rule in KNOWN_GAPS for rule, _ in problems) and not self.strict:
            status = "GAP"
        else:
            status = "FAIL"
        more = " (+%d more, --verbose shows all)" % (len(problems) - 1)
        if status == "PASS":
            text = ok
        elif status == "GAP":
            text = ("KNOWN KERNEL GAP — reported, not counted (--strict makes it fatal): %s%s — %s"
                    % (problems[0][1], "" if len(problems) == 1 or self.verbose else more, KNOWN_GAPS[problems[0][0]]))
        else:
            shown = problems if self.verbose else problems[:2]
            text = "; ".join(message for _, message in shown)
            if len(problems) > len(shown):
                text += " …" + more
        print("[%s] %s — %s" % (status, name, text))
        if status == "GAP" and self.verbose:
            # --verbose promises every problem: the rest of a gap's, one per indented line.
            for _, message in problems[1:]:
                print("    " + message)
        self.counts[status] += 1
        if status == "FAIL":
            self.failed.append(name)
        elif status == "GAP":
            self.gapped.update(rule for rule, _ in problems)
        return status != "FAIL"

    def detail(self, name):
        """A sink for per-request lines under `name` (--verbose only); None when
        there is nothing to print."""
        if not (self.verbose and self.selected(name)):
            return None
        return lambda line: print("    " + line.replace("\n", "\n    "))

    def finish(self, label, seconds):
        passed, failed, gaps = self.counts["PASS"], self.counts["FAIL"], self.counts["GAP"]
        subset = "" if self.only is None else " (subset: --only %s)" % self.only
        if failed:
            print("[FAIL] %s: %d failed (%s), %d passed, %d known gap(s)%s — %.1f s"
                  % (label, failed, ", ".join(self.failed[:3]) + (", …" if failed > 3 else ""),
                     passed, gaps, subset, seconds))
        else:
            print("[PASS] %s: %d passed, 0 failed, %d known gap(s)%s — %.1f s"
                  % (label, passed, gaps, subset, seconds))
        return 1 if failed else 0


def run_env(scratch, extra=None):
    """Environment of the binary under test: the current one, minus whatever
    could send the loopback traffic elsewhere (proxy variables), plus the two
    isolations scripts/regress_eval.py applies — XDG_CONFIG_HOME pointing into
    the scratch dir (the developer's real skill table must not leak into the
    run) and GIT_CEILING_DIRECTORIES covering the scratch dir and its parents."""
    env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1")
    for name in list(env):
        if name.lower() in PROXY_VARS:
            del env[name]
    xdg = os.path.join(scratch, ".xdg")
    os.makedirs(xdg, exist_ok=True)
    env["XDG_CONFIG_HOME"] = xdg
    scratch = os.path.realpath(scratch)
    env["GIT_CEILING_DIRECTORIES"] = os.pathsep.join([scratch] + _parents(scratch))
    env.update(extra or {})
    return env


def _parents(path):
    out, head = [], path
    while True:
        head, tail = os.path.split(head)
        if not tail:
            return out
        out.append(head)


def base_config(base_url, workdir):
    return {
        "provider": "anthropic", "base_url": base_url, "api_key_env": KEY_ENV, "model": "mock-claude",
        "max_tokens": 512, "workdir": workdir, "tools": ["write_file", "list_dir"],
        "inject_environment": False, "restrict_to_workdir": True,
        "limits": {"max_turns": 30, "timeout_secs": 60, "max_output_chars": 20000,
                   "max_context_tokens": 200000},
    }


def merge(target, overrides):
    for key, value in overrides.items():
        if isinstance(value, dict) and isinstance(target.get(key), dict):
            merge(target[key], value)
        else:
            target[key] = value


def load_json(path):
    try:
        with open(path, encoding="utf-8") as handle:
            return json.load(handle)
    except (OSError, ValueError):
        return None


def parse_ndjson(stream):
    events = []
    for line in stream.splitlines():
        line = line.strip()
        if line.startswith("{"):
            try:
                events.append(json.loads(line))
            except ValueError:
                pass
    return events


NO_TASK = object()


class Run:
    """One invocation of the binary."""

    def __init__(self, label, rc, result, events, stderr, expect_rc, expect_stop):
        self.label, self.rc, self.result, self.events = label, rc, result, events
        # The tail of stderr without advisory lines: what is left explains a failure.
        lines = [line for line in (stderr or "").splitlines()
                 if line.strip() and not line.startswith(("[warn]", "[note]", "[context]"))]
        self.stderr = " ⏎ ".join(lines)[-500:]
        self.expect_rc, self.expect_stop = expect_rc, expect_stop

    @property
    def stop_reason(self):
        return self.result.get("stop_reason") if self.result else None

    @property
    def ok(self):
        return self.rc in self.expect_rc and self.result is not None and self.stop_reason in self.expect_stop

    def describe(self):
        turns = self.result.get("turns") if self.result else "no"
        return "%s: exit %s, %s, %s turn(s)" % (self.label, self.rc, self.stop_reason or "no result.json", turns)


class Scenario:
    """One scenario: a scratch directory with its own session file, and the
    runs made against it."""

    def __init__(self, name, mock, base_url, root, env):
        self.name, self.mock, self.env = name, mock, env
        self.base_url = base_url
        self.dir = os.path.join(root, name)
        self.workdir = os.path.join(self.dir, "work")
        os.makedirs(self.workdir)
        self.session = os.path.join(self.dir, "session.json")
        self.runs, self.snapshots, self.configs = [], [], 0

    @property
    def ok(self):
        """Every run so far ended the way its scenario expects."""
        return bool(self.runs) and all(item.ok for item in self.runs)

    def config(self, **overrides):
        self.configs += 1
        config = base_config(self.base_url, self.workdir)
        merge(config, overrides)
        path = os.path.join(self.dir, "config-%d.json" % self.configs)
        with open(path, "w", encoding="utf-8") as handle:
            json.dump(config, handle, ensure_ascii=False, indent=2)
        return path

    def run(self, label, script, config, task=NO_TASK, resume=False, expect_rc=(0,), expect_stop=("end_turn",)):
        self.mock.begin("%s/%s" % (self.name, label), script)
        result_path = os.path.join(self.dir, "result-%d.json" % (len(self.runs) + 1))
        command = [BIN, "--config", config, "--session", self.session, "--json", "--output", result_path]
        if resume:
            command.append("--resume")
        if task is not NO_TASK:
            command += ["--task", task]
        try:
            proc = subprocess.run(command, cwd=self.dir, env=self.env, capture_output=True, text=True,
                                  encoding="utf-8", errors="replace", timeout=240)
        except (OSError, subprocess.TimeoutExpired) as error:
            run = Run(label, -1, None, [], "the binary could not be run: %s" % error, expect_rc, expect_stop)
        else:
            run = Run(label, proc.returncode, load_json(result_path), parse_ndjson(proc.stdout),
                      proc.stderr, expect_rc, expect_stop)
        self.runs.append(run)
        self.snapshots.append(load_json(self.session))
        return run

    def exchanges(self):
        """Everything the mock recorded for this scenario, in arrival order."""
        prefix = self.name + "/"
        with self.mock.lock:
            records = [record for record in self.mock.records if record["tag"].startswith(prefix)]
        many = len(self.runs) > 1
        out = []
        for record in records:
            where = "%s request #%d" % (record["tag"] if many else self.name, record["seq"])
            exchange = Exchange(record["n"], record["body"], record["events"], record["error"], where,
                                200 if record["events"] is not None else 400)
            exchange.label = record["tag"][len(prefix):]
            out.append(exchange)
        return out


def run(sc, label, script, overrides=None, **kwargs):
    return sc.run(label, script, sc.config(**(overrides or {})), **kwargs)


def emit_outcome(rep, sc):
    bad = ["%s — %s" % (item.describe(), item.stderr) for item in sc.runs if not item.ok]
    rep.emit("%s: run outcome" % sc.name, bad, " · ".join(item.describe() for item in sc.runs))


def emit_traffic(rep, name, audit, carry_on, quiet=""):
    """The shared wire rules: shape, pairing, echo fidelity, content, folds."""
    rules, stats = audit["rules"], audit["stats"]
    rep.emit("%s: request shape" % name, (rules.get("shape") or [])
             + [("lifetime", problem) for problem in rules.get("lifetime") or []],
             "%d request(s): roles alternate user/assistant, the conversation opens with a text block, thinking "
             "blocks only inside the running turn's assistant messages, signed, with exactly the API's keys"
             % stats["main"], gaps=("lifetime",))
    rep.emit("%s: tool pairing" % name, rules.get("pairing"),
             "%d tool_use id(s), each unique and answered N:N by the user message right after it" % stats["calls"])
    carried = sum(len(reply.reasoning) for reply in audit["replies"])
    if carry_on:
        ok = "%d reasoning block(s) re-sent byte-identical (text, signature, position) across %d request(s)" % (
            stats["expected"], stats["echo_requests"])
        if not stats["expected"]:
            ok = ("none of the %d reasoning block(s) the endpoint signed was due for an echo — each belonged to a turn "
                  "that had ended, or to the final reply (%d request(s))" % (carried, stats["main"]))
    else:
        ok = "%d reasoning block(s) re-sent in %d request(s) although the endpoint signed %d (%s)" % (
            stats["echoed"], stats["main"], carried, quiet)
    if stats["orders"]:
        ok += "; block orders " + " · ".join(stats["orders"])
    if stats["unusable"]:
        ok += "; %d request(s) with no usable response, not checked for echo" % stats["unusable"]
    notes = audit["notes"].get("unverified") or []
    if notes:
        ok += "; %d assistant message(s) unverifiable (their replies predate the capture)" % len(notes)
    rep.emit("%s: echo fidelity" % name, (rules.get("echo") or []) + (rules.get("carry") or []), ok)
    texts = sum(1 for reply in audit["replies"] for block in reply.content if block.get("type") == "text")
    rep.emit("%s: reply content" % name, (rules.get("content") or []) + (rules.get("tool") or []),
             "%d text block(s) and %d tool_use input(s) of %d repl%s re-sent unchanged (inputs compared as parsed JSON)"
             % (texts, stats["calls"], len(audit["replies"]), "y" if len(audit["replies"]) == 1 else "ies"))
    rep.emit("%s: responses" % name, rules.get("response"),
             "every request got a complete, error-free response")
    rep.emit("%s: known replies" % name, rules.get("source"),
             "every assistant message on the wire is a complete reply the endpoint sent")
    if stats["summary"]:
        rep.emit("%s: fold" % name, rules.get("summary"),
                 "%d summar%s request(s) carrying no reasoning block; the request after each is [summary head] + an "
                 "untouched assistant-led tail of the conversation the summariser saw"
                 % (stats["summary"], "y" if stats["summary"] == 1 else "ies"))


def usage_text(exchanges):
    """The four axes summed over `exchanges`, the way both usage lines print them."""
    total = sum_usage(exchanges)
    main = sum(1 for exchange in exchanges if exchange.kind == "main")
    return ("%d request(s) (%d main + %d summary): input %s / output %s / cache write %s / cache read %s"
            % (len(exchanges), main, len(exchanges) - main, format(total["input_tokens"], ","),
               format(total["output_tokens"], ","), format(total["cache_creation_input_tokens"], ","),
               format(total["cache_read_input_tokens"], ",")))


def emit_usage(rep, name, item, exchanges, many=False):
    label = "%s: usage" % name if not many else "%s: usage (%s)" % (name, item.label)
    if item.result is None:
        rep.emit(label, "no result.json was written", "")
        return
    problems = usage_problems(item.result, exchanges, "%s result.json" % item.label, expect_turns=item.ok)
    main = sum(1 for exchange in exchanges if exchange.kind == "main")
    rep.emit(label, problems,
             "%s — each axis equals result.json over the whole run; reasoning_tokens absent; turns = %d"
             % (usage_text(exchanges), main))


def emit_events(rep, name, item, exchanges, many=False):
    """The stream channel: NDJSON `thinking` deltas and `text` deltas each sum
    to exactly what the endpoint sent."""
    if item.result is None:
        return
    sent = {"thinking": "", "text": ""}
    for exchange in exchanges:
        if exchange.kind != "main" or exchange.reply is None:
            continue
        for block in exchange.reply.blocks:
            if block.get("type") in sent:
                sent[block["type"]] += block.get(block["type"]) or ""
    got = {kind: "".join(event.get("delta") or "" for event in item.events if event.get("type") == kind)
           for kind in ("thinking", "text")}
    problems = ["%s deltas (%d chars) are not what the endpoint sent (%d chars)"
                % (kind, len(got[kind]), len(sent[kind])) for kind in ("thinking", "text") if got[kind] != sent[kind]]
    label = "%s: stream channel" % name if not many else "%s: stream channel (%s)" % (name, item.label)
    rep.emit(label, problems,
             "%d thinking char(s) and %d text char(s) of %s — each NDJSON channel sums to exactly what the endpoint sent"
             % (len(sent["thinking"]), len(sent["text"]), item.label))


def emit_session(rep, name, session, audit, carry_on, note="", skip=(), strict=True):
    label = "%s: session three-way" % name if not note else "%s: session three-way (%s)" % (name, note)
    if session is None:
        return rep.emit(label, "no session file was written", "")
    problems, stats = session_problems(session, audit, carry_on, "session", strict_sources=strict, skip=skip)
    # A message whose reply predates the capture cannot be judged: a note, never a failure.
    loose = [message for rule, message in problems if rule == "unverified"]
    problems = [(rule, message) for rule, message in problems if rule != "unverified"]
    silent = ("; %d block(s) stored without an echo being due on the wire (their turn is over, or thinking is "
              "disabled)") % stats["silent"] if stats["silent"] else ""
    held = ("; %d message(s) held to the wire's echo only (their replies predate the capture)" % stats["wire_only"]
            if stats["wire_only"] else "")
    status = rep.emit(label, problems,
                      "%d assistant message(s), %d reasoning block(s): `before`, text and signature are the endpoint's; "
                      "the wire form equals the last echo (%d block(s) re-sent, %d message(s) final and never "
                      "re-sent)%s%s; no reasoning on user or archived messages; %d archived assistant message(s) match "
                      "the folded replies"
                      % (stats["messages"], stats["blocks"], stats["echoed"], stats["final"], silent, held,
                         stats["archived"]), gaps=("lifetime",))
    for message in (loose if rep.verbose else loose[:1]):
        print("[note] %s: unverified — %s%s" % (label, message, "" if rep.verbose else " (--verbose lists them)"))
    return problems


def field_problems(exchanges, want):
    """`reasoning_effort` as the provider puts it on the wire."""
    problems = []
    for exchange in exchanges:
        if exchange.kind != "main" or exchange.reply is None:
            continue
        body = exchange.body
        thinking_field, config = body.get("thinking"), body.get("output_config")
        if want is None:
            if "thinking" in body or "output_config" in body:
                problems.append("%s: carries thinking=%s / output_config=%s although reasoning_effort is unset"
                                % (exchange.where, thinking_field, config))
        elif want == "none":
            if thinking_field != {"type": "disabled"}:
                problems.append("%s: thinking is %s, expected {'type': 'disabled'}" % (exchange.where, thinking_field))
            if "output_config" in body:
                problems.append("%s: carries output_config=%s although thinking is disabled" % (exchange.where, config))
        else:
            if thinking_field != {"type": "adaptive"}:
                problems.append("%s: thinking is %s, expected {'type': 'adaptive'}" % (exchange.where, thinking_field))
            if config != {"effort": want}:
                problems.append("%s: output_config is %s, expected {'effort': %r}" % (exchange.where, config, want))
    return problems


def emit_fields(rep, name, exchanges, want):
    count = sum(1 for exchange in exchanges if exchange.kind == "main" and exchange.reply is not None)
    if want is None:
        ok = "neither `thinking` nor `output_config` in %d request(s) (reasoning_effort unset)" % count
    elif want == "none":
        ok = '`thinking` is `{"type": "disabled"}` and there is no `output_config` in %d request(s); the carry is off with it' % count
    else:
        ok = ('`thinking` is `{"type": "adaptive"}` and `output_config` is `{"effort": "%s"}` in %d request(s); '
              "the carry is unaffected") % (want, count)
    rep.emit("%s: request fields" % name, field_problems(exchanges, want), ok)


def audit_scenario(sc, rep, carry_on, quiet=""):
    """Run outcome, the shared wire rules, usage and the stream channel for a
    finished scenario. Returns (exchanges, audit)."""
    exchanges = sc.exchanges()
    audit = audit_traffic(exchanges, carry_on, strict_sources=True, detail=rep.detail(sc.name))
    emit_outcome(rep, sc)
    if not any(item.ok for item in sc.runs):
        return exchanges, audit
    emit_traffic(rep, sc.name, audit, carry_on, quiet)
    many = len(sc.runs) > 1
    for item in sc.runs:
        mine = [exchange for exchange in exchanges if exchange.label == item.label]
        emit_usage(rep, sc.name, item, mine, many)
        emit_events(rep, sc.name, item, mine, many)
    return exchanges, audit


def shapes_script(prefix):
    """One reply per block order seen on real traffic."""
    def think(n):
        return thinking(THINKING[(n - 1) % len(THINKING)], signature("%s-r%d" % (prefix, n)))

    def write(n, content):
        return tool_use("write_file", {"path": "%s-%d.txt" % (prefix, n), "content": content},
                        "toolu_%s_%d" % (prefix, n))

    return [
        Reply([think(1), write(1, "first file\n"), tool_use("list_dir", {}, "toolu_%s_list" % prefix)]),
        Reply([text("Writing the second one."), think(2),
               write(2, "second file — quotes \" and a backslash \\\n")]),
        Reply([thinking("", signature("%s-r3" % prefix)), write(3, "third file\n" * 5)]),
        Reply([think(4), text("Last write."), write(4, "fourth file\n")]),
        Reply([think(5), text("All four files are written.")]),
    ]


def two_reply_script(prefix, final_text):
    """A tool call carrying thinking, then the final answer carrying another."""
    return [
        Reply([thinking(THINKING[0], signature("%s-r1" % prefix)),
               tool_use("write_file", {"path": "%s.txt" % prefix, "content": "written by %s\n" % prefix},
                        "toolu_%s_1" % prefix)]),
        Reply([thinking(THINKING[1], signature("%s-r2" % prefix)), text(final_text)]),
    ]


def sc_shapes(sc, rep):
    run(sc, "run", shapes_script("shapes"), task="Write the four files.")
    exchanges, audit = audit_scenario(sc, rep, True)
    if not sc.ok:
        return
    emit_fields(rep, sc.name, exchanges, None)
    emit_session(rep, sc.name, sc.snapshots[-1], audit, True)


def sc_edge_blocks(sc, rep):
    script = [
        Reply([redacted(signature("edge-redacted")),
               tool_use("write_file", {"path": "edge-1.txt", "content": "one\n"}, "toolu_edge_1")]),
        Reply([thinking("an unsigned draft — the kernel must drop it", ""), thinking(THINKING[2], signature("edge-r2")),
               tool_use("write_file", {"path": "edge-2.txt", "content": "two\n"}, "toolu_edge_2")]),
        Reply([thinking(THINKING[3], signature("edge-r3")), text("Edge cases done.")]),
    ]
    run(sc, "run", script, task="Write the two edge files.")
    exchanges, audit = audit_scenario(sc, rep, True)
    if not sc.ok:
        return
    unsigned = "an unsigned draft — the kernel must drop it"
    leaked = [exchange.where for exchange in exchanges
              if any(block.get("thinking") == unsigned for message in exchange.body.get("messages") or []
                     for block in wire_content(message))]
    rep.emit("%s: unsigned block dropped" % sc.name, leaked,
             "the unsigned thinking block is never re-sent and never stored; the redacted one is re-sent as "
             "redacted_thinking with its data intact")
    emit_session(rep, sc.name, sc.snapshots[-1], audit, True)


def sc_carry_off(sc, rep):
    run(sc, "run", shapes_script("off"), {"preserved_reasoning": False}, task="Write the four files.")
    exchanges, audit = audit_scenario(sc, rep, False, quiet="preserved_reasoning off")
    if not sc.ok:
        return
    emit_session(rep, sc.name, sc.snapshots[-1], audit, False)


def sc_effort(sc, rep, level):
    run(sc, "run", shapes_script("effort-%s" % level), {"reasoning_effort": level}, task="Write the four files.")
    exchanges, audit = audit_scenario(sc, rep, True,
                                      quiet="thinking disabled by reasoning_effort none" if level == "none" else "")
    if not sc.ok:
        return
    emit_fields(rep, sc.name, exchanges, level)
    emit_session(rep, sc.name, sc.snapshots[-1], audit, True)


def sc_multi_turn(sc, rep):
    run(sc, "turn-1", two_reply_script("mt1", "Turn one done."), task="Turn one: write mt1.txt.")
    run(sc, "turn-2", two_reply_script("mt2", "Turn two done."), task="Turn two: write mt2.txt.", resume=True)
    run(sc, "turn-3", [Reply([thinking(THINKING[2], signature("mt3-r1")), text("Turn three done.")])],
        task="Turn three: just answer.", resume=True)
    exchanges, audit = audit_scenario(sc, rep, True)
    if not sc.ok:
        return
    # The lifetime rule, on the wire and in every session snapshot after a new turn opened.
    stale = [("lifetime", problem) for problem in audit["rules"].get("lifetime") or []]
    for item, snapshot in zip(sc.runs[1:], sc.snapshots[1:]):
        if snapshot is not None:
            stale += [(rule, problem) for rule, problem in
                      session_problems(snapshot, audit, True, "session after %s" % item.label, True)[0]
                      if rule == "lifetime"]
    rep.emit("%s: new user turn ends the carry" % sc.name, stale,
             "%d user turn(s) over one session: inside a turn every later request re-sends that turn's blocks; once a "
             "user message opens the next turn, none of the earlier ones is re-sent or stored any more"
             % len(sc.runs), gaps=("lifetime",))
    emit_session(rep, sc.name, sc.snapshots[-1], audit, True, skip=("lifetime",))


def sc_resume(sc, rep):
    def forever(n, script):
        return Reply([thinking(THINKING[n % len(THINKING)], signature("resume-r%d" % n)),
                      tool_use("write_file", {"path": "resume-%d.txt" % n, "content": "part %d\n" % n},
                               "toolu_resume_%d" % n)])

    run(sc, "cut", Script(dynamic=forever), {"limits": {"max_turns": 1}}, task="Start writing the parts.",
        expect_rc=(1,), expect_stop=("max_turns_reached",))
    run(sc, "resume", [Reply([thinking(THINKING[4], signature("resume-final")), text("Resumed and finished.")])],
        resume=True)
    exchanges, audit = audit_scenario(sc, rep, True)
    if not sc.ok:
        return
    problems = []
    first = sc.snapshots[0]
    if first is None:
        problems.append("no session file was written by the cut run")
    else:
        carriers = [i for i, message in enumerate(first.get("messages") or []) if "reasoning" in message]
        if carriers != [1]:
            problems.append("after the cut run the messages carrying `reasoning` are %s, expected [1]" % carriers)
        resumed = [exchange for exchange in exchanges if exchange.label == "resume"]
        echoed = sum(len(split_reasoning(wire_content(message))[0])
                     for exchange in resumed for message in exchange.body.get("messages") or []
                     if message.get("role") == "assistant")
        if echoed != 1:
            problems.append("the resumed process re-sent %d reasoning block(s), expected the 1 the cut run stored" % echoed)
    rep.emit("%s: cross-process carry" % sc.name, problems,
             "the cut run's thinking block survives the session round-trip: stored on the one assistant message, "
             "re-sent byte-identical by the next process (`--resume` without a new task stays inside the turn)")
    for item, snapshot in zip(sc.runs, sc.snapshots):
        emit_session(rep, sc.name, snapshot, audit, True, note="after %s" % item.label)


def carriers(session):
    """Indexes of a session's messages that carry `reasoning`."""
    return [i for i, message in enumerate((session or {}).get("messages") or [])
            if isinstance(message, dict) and "reasoning" in message]


def salvage_events(item):
    """max_tokens hit once: exactly one NDJSON `salvage` event."""
    count = sum(1 for event in item.events if event.get("type") == "salvage")
    return [] if count == 1 else ["%s: %d NDJSON salvage event(s), expected 1" % (item.label, count)]


def sc_salvage(sc, rep):
    """salvage_on_max_tokens after a cut reply that had text. The partial text
    is in the history, so the one-shot wrap-up nudge is pushed as a user
    message of its own: it opens a new turn, and the cut reply's carry ends
    with the old one — the lifetime rule seen the right way round."""
    script = [
        Reply([thinking(THINKING[0], signature("salvage-cut")),
               text("The first half of the answer, cut off a")], "max_tokens"),
        Reply([thinking(THINKING[1], signature("salvage-final")), text("…nd here is the rest.")]),
    ]
    run(sc, "run", script, task="Answer in one long paragraph.")
    exchanges, audit = audit_scenario(sc, rep, True)
    if not sc.ok:
        return
    rep.emit("%s: salvage event" % sc.name, salvage_events(sc.runs[0]),
             "max_tokens was hit once and answered with exactly one wrap-up turn (one NDJSON `salvage` event)")
    problems = []
    first = exchanges[0] if exchanges else None
    second = exchanges[1] if len(exchanges) > 1 else None
    if first is None or second is None or first.reply is None:
        problems.append("%d request(s) reached the mock, expected 2 (the cut reply, then the wrap-up turn)"
                        % len(exchanges))
    else:
        if first.reply.stop_reason != "max_tokens" or not first.reply.reasoning:
            problems.append("the first reply is not a max_tokens cut carrying a signed thinking block")
        later = second.body.get("messages") or []
        roles = [message.get("role") for message in later]
        carried = sum(len(split_reasoning(wire_content(message))[0]) for message in later)
        if roles != ["user", "assistant", "user"]:
            problems.append("the wrap-up request has the roles %s, expected [user, assistant, user]" % roles)
        elif carried:
            problems.append("the cut reply's %d reasoning block(s) are still re-sent in the wrap-up request" % carried)
        elif not is_turn_opener(later[-1]) or not any(block.get("type") == "text" for block in wire_content(later[-1])):
            problems.append("the wrap-up nudge is not a text block in a user message of its own")
        keepers = carriers(sc.snapshots[0])
        if keepers != [3]:
            problems.append("after the run the session messages carrying `reasoning` are %s, expected [3] — the cut "
                            "reply's block ended with its turn, only the wrap-up reply's is kept" % keepers)
    rep.emit("%s: wrap-up after a cut reply" % sc.name, problems,
             "the nudge is a user message of its own and opens a new turn: the cut reply's signed block is neither "
             "re-sent nor stored after it; the wrap-up reply's block is stored (final, never re-sent)")
    emit_session(rep, sc.name, sc.snapshots[-1], audit, True)


def sc_salvage_empty(sc, rep):
    """salvage_on_max_tokens after an empty reply (a thinking block and nothing
    else): nothing entered the history, so the nudge merges into the trailing
    tool-result message — no new turn, and the loop's carry continues."""
    script = [
        Reply([thinking(THINKING[2], signature("salvage-tool")),
               tool_use("write_file", {"path": "a.txt", "content": "x\n"}, "toolu_salvage_1")]),
        Reply([thinking(THINKING[3], signature("salvage-empty"))], "max_tokens"),
        Reply([thinking(THINKING[4], signature("salvage-done")), text("Done after the nudge.")]),
    ]
    run(sc, "run", script, task="Write a.txt, then answer.")
    exchanges, audit = audit_scenario(sc, rep, True)
    if not sc.ok:
        return
    rep.emit("%s: salvage event" % sc.name, salvage_events(sc.runs[0]),
             "max_tokens was hit once and answered with exactly one wrap-up turn (one NDJSON `salvage` event)")
    problems = []
    if len(exchanges) != 3:
        problems.append("%d request(s) reached the mock, expected 3 (the tool call, the empty reply, the wrap-up turn)"
                        % len(exchanges))
    else:
        cut = exchanges[1].reply
        if cut is None or cut.stop_reason != "max_tokens" or cut.content or not cut.reasoning:
            problems.append("the second reply is not an empty max_tokens cut (a thinking block and nothing else)")
        messages = exchanges[-1].body.get("messages") or []
        kinds = [block.get("type") for block in wire_content(messages[-1])] if messages else []
        if (len(messages) != 3
                or [message.get("role") for message in messages] != ["user", "assistant", "user"]):
            problems.append("the wrap-up request has %d message(s) with the roles %s, expected 3 as "
                            "[user, assistant, user]" % (len(messages), [m.get("role") for m in messages]))
        elif "tool_result" not in kinds or "text" not in kinds:
            problems.append("the wrap-up nudge did not merge into the trailing tool-result message (block types there: "
                            "%s)" % kinds)
        elif turn_indices(messages)[-1] != 1:
            problems.append("the wrap-up nudge opened a new turn, expected the loop's turn to continue")
        elif len(split_reasoning(wire_content(messages[1]))[0]) != 1:
            problems.append("the tool call's thinking block is not re-sent in the wrap-up request")
        keepers = carriers(sc.snapshots[-1])
        if keepers != [1, 3]:
            problems.append("after the run the session messages carrying `reasoning` are %s, expected [1, 3] (the tool "
                            "call's block and the final reply's)" % keepers)
    rep.emit("%s: wrap-up after an empty reply" % sc.name, problems,
             "no assistant message for the empty reply, so the nudge merges into the trailing tool-result message: no "
             "new turn — the tool call's block is still re-sent byte-identical, and both blocks stay stored")
    emit_session(rep, sc.name, sc.snapshots[-1], audit, True)


PAYLOAD = ("".join("line %04d: the quick brown fox jumps over the lazy dog 0123456789\n" % i
                   for i in range(200)))[:8000]
SUMMARY_USAGE = {"input_tokens": 900, "output_tokens": 70, "cache_creation_input_tokens": 0,
                 "cache_read_input_tokens": 0}


def sc_compression(sc, rep):
    def dynamic(n, script):
        if script.summaries:                        # the fold happened: wrap up
            return Reply([thinking(THINKING[4], signature("cmp-final")),
                          text("The earlier rounds are summarised; every part is written.")])
        note = "Round %d: append part %02d of the log (%d characters).\n%s" % (
            n, n, len(PAYLOAD), THINKING[n % len(THINKING)].strip())
        return Reply([thinking(note, signature("cmp-r%d" % n)),
                      tool_use("write_file", {"path": "part-%02d.txt" % n, "content": PAYLOAD}, "toolu_cmp_%d" % n)])

    summary = Reply([text("SUMMARY-MARK: the model wrote one part file per round into the workdir, and nothing else "
                          "happened.")], "end_turn", SUMMARY_USAGE)
    run(sc, "run", Script(summary=summary, dynamic=dynamic),
        {"limits": {"max_context_tokens": 24000, "keep_recent_messages": 2, "max_turns": 40}},
        task="Write the parts one by one until you are told to stop.")
    exchanges, audit = audit_scenario(sc, rep, True)
    if not sc.ok:
        return
    facts, stats = audit["folds"], audit["stats"]
    problems, fact, wires = [], None, None
    if not facts:
        problems.append("no summary request reached the mock — the fold never tripped")
    else:
        fact = facts[0]
        wires = fact["facts"]
        if stats["summary"] != 1:
            problems.append("%d summary request(s), expected exactly 1" % stats["summary"])
        if fact["before"] < 3:
            problems.append("the fold tripped after only %d main-loop request(s) — too early to pin anything"
                            % fact["before"])
        post = fact["post"].body.get("messages") or []
        if [message.get("role") for message in post] != ["user", "assistant", "user"]:
            problems.append("the request after the summary has roles %s, expected [user, assistant, user] (the summary "
                            "head + the kept round)" % [message.get("role") for message in post])
        elif not split_reasoning(wire_content(post[1]))[0]:
            problems.append("the kept round's assistant message carries no thinking block after the fold")
        if stats["main"] - fact["before"] != 1:
            problems.append("%d main-loop request(s) after the fold, expected exactly 1 (the final answer)"
                            % (stats["main"] - fact["before"]))
    rep.emit("%s: fold timing" % sc.name, problems,
             "1 summary request after %d completed round(s); the request after it is [summary head, assistant, user] — "
             "the kept round still carries its thinking block, the folded ones are gone; then one final request"
             % (fact["before"] if fact else 0))
    problems = []
    records = (sc.runs[0].result or {}).get("compressions") or []
    folded = wires["folded"] if wires else None
    if len(records) != 1:
        problems.append("result.json carries %d compression record(s), expected 1" % len(records))
    elif folded is None:
        problems.append("the wire shows no fold the record could be checked against")
    else:
        record = records[0]
        archive = (sc.snapshots[-1] or {}).get("archive") or []
        ids = sorted(record.get("folded_ids") or [])
        if record.get("trigger") != "preflight":
            problems.append("the record's trigger is %r, expected 'preflight'" % record.get("trigger"))
        if record.get("kept") != wires["after"]:
            problems.append("the record keeps %s message(s), the wire shows %s" % (record.get("kept"), wires["after"]))
        if record.get("dropped") != folded or len(ids) != folded:
            problems.append("the record drops %s message(s) in %d id(s), the wire shows %d folded"
                            % (record.get("dropped"), len(ids), folded))
        if len(archive) != folded or sorted(str(item.get("id")) for item in archive) != ids:
            problems.append("the session archive holds %d message(s) with other ids than the record's %d folded ones"
                            % (len(archive), folded))
        if not (record.get("estimated_tokens_after") or 0) < (record.get("estimated_tokens_before") or 0):
            problems.append("the record's estimate did not shrink: before %s, after %s"
                            % (record.get("estimated_tokens_before"), record.get("estimated_tokens_after")))
    rep.emit("%s: compression record" % sc.name, problems,
             "result.json compressions == 1 × preflight (dropped %s, kept %s) — the same fold the wire shows; the "
             "session archive holds exactly the %s folded messages"
             % (folded, wires["after"] if wires else None, folded))
    emit_session(rep, sc.name, sc.snapshots[-1], audit, True, note="after the fold")


SCENARIOS = (
    ("shapes", sc_shapes),
    ("edge-blocks", sc_edge_blocks),
    ("carry-off", sc_carry_off),
    ("effort-none", lambda sc, rep: sc_effort(sc, rep, "none")),
    ("effort-low", lambda sc, rep: sc_effort(sc, rep, "low")),
    ("multi-turn", sc_multi_turn),
    ("resume", sc_resume),
    ("salvage-cut", sc_salvage),
    ("salvage-empty", sc_salvage_empty),
    ("compression", sc_compression),
)


def cmd_check(args):
    rep = Reporter(verbose=args.verbose, only=args.only, strict=args.strict)
    started = time.time()
    if not os.path.isfile(BIN) or not os.access(BIN, os.X_OK):
        rep.emit("setup: binary", "binary not found or not executable: %s (build it with `cargo build --locked "
                                  "--features code`, or point TA_BIN at a built binary) — nothing was checked" % BIN, "")
        return rep.finish("e2e_anthropic_wire", time.time() - started)
    mock = Mock()
    server = MockServer(("127.0.0.1", 0), mock)
    threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.2}, daemon=True).start()
    scratch = tempfile.mkdtemp(prefix="ta-wire-")
    try:
        rep.emit("self-test: mutations", self_test(), SELF_TEST_OK, always=True)
        env = run_env(scratch, {KEY_ENV: "dummy"})
        for name, build in SCENARIOS:
            if not rep.selected(name):
                continue
            scenario = Scenario(name, mock, server.base_url, scratch, env)
            try:
                build(scenario, rep)
            except Exception as error:                # a checker bug must be loud, never silent
                rep.emit("%s: checker" % name, "%s: %s" % (type(error).__name__, error), "no internal error")
                if rep.verbose:
                    traceback.print_exc()
    finally:
        server.shutdown()
        server.server_close()
        if args.keep:
            print("[note] scratch kept: %s" % scratch)
        else:
            shutil.rmtree(scratch, ignore_errors=True)
    if rep.only is None and not rep.strict:
        # Strict xfail: a filed gap that no longer reproduces must be retired
        # here, or the list becomes where fixed bugs go to be forgotten.
        for rule in KNOWN_GAPS:
            if rule not in rep.gapped:
                rep.emit("known gap: %s" % rule, "it no longer reproduces — retire it from KNOWN_GAPS", "")
    return rep.finish("e2e_anthropic_wire", time.time() - started)


# ── capture: a loopback forwarding proxy that records both directions ─────────
#
# Credential hygiene is the design centre here, not an afterthought:
#   * it listens on loopback only — credentials pass through it;
#   * the upstream host is used for the connection and never recorded or printed;
#   * credential-looking request headers are recorded by NAME only;
#   * the request path is recorded without its query string;
#   * every line is scanned for key patterns before it is written (a hit is
#     redacted and counted) and the three files are scanned again on close;
#   * an upstream failure is recorded as the exception class, never its message.
# There is deliberately no "insecure TLS" switch: a private CA goes into the
# interpreter's trust store (SSL_CERT_FILE), not into this proxy.

HOP_BY_HOP = {"connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer",
              "transfer-encoding", "upgrade", "host", "content-length", "accept-encoding"}
BODY_CAP = 64 * 1024       # a non-stream body is kept up to this many bytes
READ_CAP = 64 * 1024       # bytes relayed per loop: the unit in which a stream stays live
CAPTURE_FILES = ("requests.jsonl", "responses.jsonl", "meta.json")
DRAIN_SECS = 2.0            # how long stop() lets a request in flight finish recording
DEBUG = os.environ.get("TA_WIRE_DEBUG", "") not in ("", "0", "false", "no")


def forward_headers(headers):
    """[(name, value)] to send upstream: every end-to-end header as it came —
    `anthropic-version` and `anthropic-beta` included — minus hop-by-hop
    headers and the ones the connection re-derives (host, length, encoding:
    the proxy asks for identity encoding itself, it has to read the SSE text
    to frame it)."""
    drop = set(HOP_BY_HOP)
    for name, value in headers.items():
        if name.lower() == "connection":
            drop.update(part.strip().lower() for part in value.split(",") if part.strip())
    return [(name, value) for name, value in headers.items() if name.lower() not in drop]


def parse_listen(value):
    """(host, port) from HOST:PORT or PORT. Loopback only — anything else is
    refused before a socket is opened."""
    host, _, port = value.rpartition(":")
    if not host:
        host, port = "127.0.0.1", value
    try:
        number = int(port)
    except ValueError:
        raise ValueError("--listen must be HOST:PORT or PORT")
    if host not in ("127.0.0.1", "localhost"):
        raise ValueError("the capture proxy listens on loopback only: use 127.0.0.1 or localhost")
    if not 0 <= number <= 65535:
        raise ValueError("the listen port must be 0 (any free port) to 65535")
    return host, number


class Upstream:
    """Where the proxy forwards to. The host is used for the connection only —
    never recorded, printed or echoed in an error message."""

    SHAPE = "--upstream must be an http(s) base URL: https://HOST[/prefix] or http://127.0.0.1:PORT"

    def __init__(self, base_url, timeout):
        try:
            parts = urlsplit(base_url if "://" in base_url else "http://" + base_url)
            port = parts.port
        except ValueError:
            raise ValueError(self.SHAPE)
        self.secure = parts.scheme.lower() == "https"
        if parts.scheme.lower() not in ("http", "https") or not parts.hostname:
            raise ValueError(self.SHAPE)
        if parts.query or parts.fragment:
            raise ValueError("--upstream must be a base URL without a query or a fragment")
        self.host = parts.hostname
        self.port = port or (443 if self.secure else 80)
        self.prefix = parts.path.rstrip("/")
        self.timeout = timeout

    def connect(self):
        if self.secure:
            return http.client.HTTPSConnection(self.host, self.port, timeout=self.timeout)
        return http.client.HTTPConnection(self.host, self.port, timeout=self.timeout)

    def target(self, path):
        return self.prefix + path


def build_request_record(number, method, path, headers, raw):
    """One requests.jsonl line. `headers` is what would be forwarded, so the
    record holds the end-to-end headers only — credential-looking ones by NAME
    (safe_headers). The body is kept as parsed JSON when it is JSON (Messages
    API requests are), else as a capped text excerpt."""
    record = {"n": number, "at": utcnow(), "method": method, "path": path.split("?", 1)[0],
              "headers": safe_headers(headers)}
    try:
        record["body"] = json.loads(raw.decode("utf-8"))
    except (ValueError, UnicodeDecodeError):
        record["body_text"] = raw[:BODY_CAP].decode("utf-8", "replace")
        record["body_truncated"] = len(raw) > BODY_CAP
    return record


def build_response_record(number, status, reason, headers, events, reply, body_text, truncated, error):
    """One responses.jsonl line: the SSE frames exactly as they arrived
    ([event, raw data]), the reply they reassemble to (blocks, stop_reason,
    usage), or else the non-stream body (capped)."""
    record = {"n": number, "at": utcnow(), "status": status, "reason": reason,
              "headers": safe_headers(headers),
              "complete": bool(reply is not None and reply.stored and not error),
              "truncated": bool(truncated), "error": error}
    if events:
        record["events"] = [[event, data] for event, data in events]
    if reply is not None:
        record["reply"] = {"blocks": reply.blocks, "stop_reason": reply.stop_reason, "usage": reply.usage}
    if body_text:
        record["body_text"] = body_text
    return record


def scan_capture(directory):
    """{file: [pattern names]} over the capture files of `directory` — names of
    the patterns that occur, never what matched."""
    hits = {}
    for name in CAPTURE_FILES:
        try:
            with open(os.path.join(directory, name), encoding="utf-8") as handle:
                kinds = find_secrets(handle.read())
        except OSError:
            continue
        if kinds:
            hits[name] = kinds
    return hits


class CaptureState:
    """What the proxy writes, and the only place it writes it. Every record
    goes through write(): the serialised line is scanned for key patterns
    first — a hit is redacted and counted — and close() scans the files again."""

    def __init__(self, out_dir):
        self.dir = os.path.abspath(out_dir)
        os.makedirs(self.dir, exist_ok=True)
        # "x": a capture is never overwritten — a typo must not cost the only copy.
        self._handles = {}
        try:
            for name in CAPTURE_FILES[:2]:
                self._handles[name] = open(os.path.join(self.dir, name), "x", encoding="utf-8")
        except OSError:
            for handle in self._handles.values():
                handle.close()
            raise
        self.lock = threading.Lock()
        self.number = 0             # requests taken in (their record number)
        self.active = 0             # requests being forwarded right now
        self.drained = threading.Condition(self.lock)
        self.lines = {"requests.jsonl": 0, "responses.jsonl": 0}
        self.count = {"requests": 0, "responses": 0, "complete": 0, "redacted": 0}
        self.redacted = []            # {file, line, patterns}: where a key pattern was scrubbed — never what matched
        self.secret_names = set()     # NAMES of credential-looking request headers seen
        self.started = utcnow()
        self.closed = False
        self._write_meta({})

    def next_number(self):
        with self.lock:
            self.number += 1
            return self.number

    def note_headers(self, headers):
        """Remember the NAMES of the credential-looking headers among [(name, value)]."""
        with self.lock:
            self.secret_names.update(name.lower() for name, _ in headers if SECRET_HEADER.search(name.lower()))

    def enter(self):
        """A request is being forwarded (stop() waits a moment for these)."""
        with self.lock:
            self.active += 1

    def leave(self):
        with self.lock:
            self.active -= 1
            if not self.active:
                self.drained.notify_all()

    def write(self, which, record):
        """Append one record to `which` file; returns the names of the key
        patterns it matched (and scrubbed). A record offered after close() is
        dropped: the capture has stopped."""
        line = json.dumps(record, ensure_ascii=False)
        kinds = find_secrets(line)
        side = "responses" if which == "responses.jsonl" else "requests"
        with self.lock:
            if self.closed:
                return []
            if kinds:
                line = scrub(line)
                self.count["redacted"] += 1
                self.redacted.append({"file": which, "line": self.lines[which] + 1, "patterns": kinds})
            self._handles[which].write(line + "\n")
            self._handles[which].flush()
            self.lines[which] += 1
            self.count[side] += 1
            self.count["complete"] += 1 if side == "responses" and record.get("complete") else 0
        return kinds

    def _write_meta(self, hits):
        meta = {
            "tool": "scripts/e2e_anthropic_wire.py capture",
            "started_at": self.started,
            "ended_at": utcnow() if self.closed else None,
            "upstream": "<not recorded>",
            "counts": dict(self.count),
            "files": {
                "requests.jsonl": "one line per request: method, path without its query, end-to-end header names "
                                  "(credential-looking values are never recorded), the JSON body",
                "responses.jsonl": "one line per response: the SSE frames as they arrived and the reply they "
                                   "reassemble to (blocks / stop_reason / usage), or the non-stream body",
            },
            "credential_header_names_seen": sorted(self.secret_names),
            "leak_scan": {"patterns": [name for name, _ in SECRET_PATTERNS],
                          "redacted_records": list(self.redacted), "hits_in_files": hits},
        }
        with open(os.path.join(self.dir, "meta.json"), "w", encoding="utf-8") as handle:
            json.dump(meta, handle, ensure_ascii=False, indent=2)
            handle.write("\n")

    def close(self):
        """Close the files and re-scan them. Returns {file: [pattern names]}."""
        with self.lock:
            for handle in self._handles.values():
                handle.close()
            self.closed = True
        hits = scan_capture(self.dir)
        self._write_meta(hits)
        return hits


def _request_path(target):
    """The path (with its query) of a request-target, whether it came in
    origin form (/v1/messages) or absolute form (http://host/v1/messages)."""
    parts = urlsplit(target)
    return (parts.path or "/") + ("?" + parts.query if parts.query else "")


class ProxyHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    timeout = 120                # per socket operation: a stalled side never pins a thread

    def log_message(self, *args):
        pass                      # the default log prints client addresses: nothing to gain

    def handle(self):
        try:
            super().handle()
        except (OSError, http.client.HTTPException):    # a reset or timed-out keep-alive connection
            self.close_connection = True

    def _read_body(self):
        encoding = (self.headers.get("Transfer-Encoding") or "").lower()
        if "chunked" in encoding:
            out = bytearray()
            while True:
                size = int(self.rfile.readline(65536).split(b";", 1)[0].strip() or b"0", 16)
                if not size:
                    while self.rfile.readline(65536).strip():      # the trailers
                        pass
                    return bytes(out)
                out += self.rfile.read(size)
                self.rfile.readline(65536)
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length > 0 else b""

    def _proxy(self):
        state = self.server.state
        state.enter()
        try:
            self._forward()
        finally:
            state.leave()

    def _forward(self):
        state, upstream = self.server.state, self.server.upstream
        try:
            raw = self._read_body()
        except (OSError, ValueError):
            self.close_connection = True
            return
        number = state.next_number()
        forwarded = forward_headers(self.headers)
        state.note_headers(forwarded)
        # Origin form even when the request line named the proxy (absolute form);
        # the query goes upstream and is left out of the record.
        path = _request_path(self.path)
        state.write("requests.jsonl",
                    build_request_record(number, self.command, path, dict(forwarded), raw))
        target = upstream.target(path)
        conn = upstream.connect()
        try:
            try:
                conn.putrequest(self.command, target)    # Host and Accept-Encoding are derived for the upstream
                for name, value in forwarded:
                    conn.putheader(name, value)
                if raw or "Content-Length" in self.headers or self.command.upper() in ("POST", "PUT", "PATCH"):
                    conn.putheader("Content-Length", str(len(raw)))
                conn.endheaders(raw or None)
                response = conn.getresponse()
            except (OSError, http.client.HTTPException) as failure:
                # The exception message can carry the upstream host: keep the class only.
                error = type(failure).__name__
                self._send_error(502, "upstream_unreachable", error)
                state.write("responses.jsonl", build_response_record(
                    number, 502, "Bad Gateway", {"content-type": "application/json"}, [], None, "", False, error))
                return
            self._relay(response, number)
        finally:
            conn.close()

    def _relay(self, response, number):
        """Stream the upstream response through piece by piece — never buffered
        — while framing the SSE events for the record."""
        state = self.server.state
        streaming = "text/event-stream" in (response.getheader("Content-Type") or "").lower()
        headers = [(name, value) for name, value in response.getheaders() if name.lower() not in HOP_BY_HOP]
        length = response.getheader("Content-Length")
        chunked = length is None or not length.isdigit()
        decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
        framer, events, body = SseFramer(), [], bytearray()
        truncated, error = False, None
        try:
            self.send_response(response.status, response.reason)
            for name, value in headers:
                self.send_header(name, value)
            if chunked:
                self.send_header("Transfer-Encoding", "chunked")
            else:
                self.send_header("Content-Length", length)
            self.end_headers()
            while True:
                try:
                    piece = response.read1(READ_CAP)
                except (OSError, http.client.HTTPException) as failure:
                    truncated, error = True, "upstream: " + type(failure).__name__
                    break
                if not piece:
                    break
                if streaming:
                    events += framer.feed(decoder.decode(piece))
                if len(body) < BODY_CAP:
                    body += piece[:BODY_CAP - len(body)]
                self.wfile.write(("%x\r\n" % len(piece)).encode("ascii") + piece + b"\r\n" if chunked else piece)
        except OSError:                              # the client went away mid-stream
            truncated, error = True, "client disconnected"
            self.close_connection = True
        finally:
            response.close()
        if streaming:
            events += framer.feed(decoder.decode(b"", True)) + framer.finish()
        reply = Reply.from_stream(events, str(number)) if events else None
        text = "" if streaming else body.decode("utf-8", "replace")
        state.write("responses.jsonl", build_response_record(
            number, response.status, response.reason, dict(headers), events, reply, text,
            truncated or len(body) >= BODY_CAP, error))
        if not chunked:
            return
        if error:
            self.close_connection = True      # a cut stream must not look complete to the client
            return
        try:
            self.wfile.write(b"0\r\n\r\n")
        except OSError:
            self.close_connection = True

    def _send_error(self, status, kind, detail):
        payload = json.dumps({"type": "error", "error": {"type": kind, "message": detail}}).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        try:
            self.wfile.write(payload)
        except OSError:
            self.close_connection = True


for _method in ("POST", "GET", "HEAD", "PUT", "PATCH", "DELETE", "OPTIONS"):
    setattr(ProxyHandler, "do_" + _method, ProxyHandler._proxy)


class ProxyServer(LoopbackServer):
    def __init__(self, address, state, upstream):
        self.state, self.upstream = state, upstream
        super().__init__(address, ProxyHandler)

    def handle_error(self, request, client_address):
        # A bug in the proxy must be visible — but exception messages can carry
        # the upstream host (an SSL verification error names it), so the class
        # is printed and the traceback only under TA_WIRE_DEBUG=1.
        print("[capture] proxy thread failed with %s" % type(sys.exc_info()[1]).__name__, flush=True)
        if DEBUG:
            traceback.print_exc()


class CaptureProxy:
    """The capture proxy as an object — start() it, point a scratch config at
    `.base_url`, stop() it. stop() returns the leak scan ({file: [pattern names]})."""

    def __init__(self, out_dir, upstream_url, listen="127.0.0.1:0", upstream_timeout=600.0):
        host, port = parse_listen(listen)
        upstream = Upstream(upstream_url, upstream_timeout)
        self.host, self.listen_port, self.upstream = host, port, upstream
        self.state = CaptureState(out_dir)
        self.server = None

    @property
    def port(self):
        """The port actually listening (the requested one before start())."""
        return self.server.server_address[1] if self.server else self.listen_port

    @property
    def base_url(self):
        """What goes into the scratch config's `base_url`."""
        return "http://%s:%d" % (self.host, self.port)

    def start(self):
        self.server = ProxyServer((self.host, self.listen_port), self.state, self.upstream)
        threading.Thread(target=self.server.serve_forever, kwargs={"poll_interval": 0.2}, daemon=True).start()
        return self

    def stop(self):
        """Shut the listener down, close the files, re-scan them. Requests still
        in flight get DRAIN_SECS to record their (cut) response; whatever they
        offer after that is dropped, not written to a closed file."""
        self.server.shutdown()
        self.server.server_close()
        with self.state.lock:
            self.state.drained.wait_for(lambda: not self.state.active, timeout=DRAIN_SECS)
        return self.state.close()


def cmd_capture(args):
    try:
        proxy = CaptureProxy(args.out, args.upstream, args.listen, args.upstream_timeout)
    except FileExistsError:
        print("[FAIL] capture: setup — %s already holds a capture (one is never overwritten); pass a new or empty "
              "--out directory" % args.out)
        return 1
    except (OSError, ValueError) as error:
        # ValueError comes from parse_listen / Upstream: neither message carries
        # the address it rejected.
        print("[FAIL] capture: setup — %s" % error)
        return 1
    try:
        proxy.start()
    except OSError as error:
        proxy.state.close()
        print("[FAIL] capture: listen — %s" % (error.strerror or type(error).__name__))
        return 1

    def stop(signum, frame):
        raise KeyboardInterrupt

    previous = {}
    for name in ("SIGTERM", "SIGINT", "SIGHUP"):
        if hasattr(signal, name):
            try:
                previous[getattr(signal, name)] = signal.signal(getattr(signal, name), stop)
            except (ValueError, OSError):      # not the main thread, or not permitted
                pass
    # flush: a piped stdout is block-buffered, and the first line is what the
    # operator is waiting for (the base_url for the scratch config).
    print("[capture] base_url for the scratch config: %s" % proxy.base_url, flush=True)
    print("[capture] recording into %s — Ctrl-C to stop" % proxy.state.dir, flush=True)
    print("[capture] the upstream is never recorded; credential-looking headers are kept by name only", flush=True)
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        pass
    finally:
        for number, handler in previous.items():
            if handler is not None:             # None = installed from C: there is nothing to give back
                signal.signal(number, handler)
    hits = proxy.stop()
    print(capture_summary(proxy.state.count, proxy.state.secret_names, proxy.state.dir))
    problems = capture_verdict(proxy.state.count["redacted"], hits)
    Reporter().emit("capture: leak scan", problems,
                    "no key pattern in %s (0 record(s) needed redacting)" % ", ".join(CAPTURE_FILES))
    return 1 if problems else 0


def capture_summary(counts, secret_names, directory):
    """The lines `capture` prints once it stopped recording."""
    return ("[capture] %d request(s) recorded, %d answered (%d complete stream(s)); credential-looking header name(s) "
            "seen: %s\n[capture] audit it offline: python3 scripts/e2e_anthropic_wire.py audit %s [--session …] "
            "[--result …]" % (counts["requests"], counts["responses"], counts["complete"],
                              ", ".join(sorted(secret_names)) or "none", directory))


def capture_verdict(redacted, hits):
    """Problems of the leak verdict ([] = clean, exit 0). `redacted`: records
    that matched a key pattern and were scrubbed before writing; `hits`: what
    the final scan of the files still finds."""
    problems = []
    if redacted:
        problems.append("%d record(s) matched a key pattern and were redacted before writing (where: meta.json → "
                        "leak_scan.redacted_records) — nothing was written in clear; inside the scrubbed text the "
                        "audit compares redacted copies, not the endpoint's bytes" % redacted)
    problems.extend("%s still matches the %s pattern — a key is in the capture directory: delete it, and rotate what "
                    "leaked" % (name, " / ".join(kinds)) for name, kinds in sorted(hits.items()))
    return problems


# ── audit: the same checks over a capture directory ───────────────────────────


def read_jsonl(path):
    """[record per line] of a capture file; a line that does not parse stays
    in its place as None. None when the file cannot be read."""
    try:
        with open(path, encoding="utf-8") as handle:
            lines = handle.read().splitlines()
    except OSError:
        return None
    out = []
    for line in lines:
        if not line.strip():
            continue
        try:
            out.append(json.loads(line))
        except ValueError:
            out.append(None)
    return out


def recorded_events(response):
    """[(event, raw data)] of a response record — the frames as they arrived."""
    out = []
    for frame in response.get("events") or []:
        if isinstance(frame, (list, tuple)) and len(frame) == 2:
            out.append((_str(frame[0]), frame[1] if isinstance(frame[1], str) else json.dumps(frame[1])))
    return out


def capture_exchanges(directory):
    """A capture directory as the same exchanges `check` audits: requests and
    responses paired by their number, the replies reassembled from the recorded
    SSE frames by the shared reader. Returns (exchanges, [problems])."""
    requests = read_jsonl(os.path.join(directory, "requests.jsonl")) or []
    responses = read_jsonl(os.path.join(directory, "responses.jsonl")) or []
    problems, answered = [], {}
    for index, record in enumerate(responses):
        if isinstance(record, dict) and isinstance(record.get("n"), int):
            answered.setdefault(record["n"], record)
        else:
            problems.append("responses.jsonl line %d is not a response record" % (index + 1))
    exchanges = []
    for index, record in enumerate(requests):
        if not (isinstance(record, dict) and isinstance(record.get("n"), int)):
            problems.append("requests.jsonl line %d is not a request record" % (index + 1))
            continue
        response = answered.get(record["n"])
        status = response.get("status") if isinstance(response, dict) else None
        body, note = record.get("body"), None
        if not isinstance(body, dict):
            body, note = {}, "the request body is not a JSON object"
        events = None
        if isinstance(response, dict) and status == 200 and isinstance(response.get("events"), list):
            events = recorded_events(response)             # an empty stream is still a (cut) stream
            note = None if response.get("complete") else "the recorded stream is marked incomplete"
        elif isinstance(response, dict):
            note = "HTTP %s" % status
        else:
            note = "no response was recorded"
        exchange = Exchange(record["n"], body, events, note, "request #%s" % record["n"],
                            status if isinstance(status, int) else 0)
        exchange.label = str(record["n"])
        exchanges.append(exchange)
    return exchanges, problems


def usage_window(result, exchanges, start):
    """End index (exclusive) of the first run of requests from `start` on whose
    summed usage is what this result.json reports — and, when the file has
    `turns`, whose number of main-loop requests is that too. None when no run
    matches; a run that matches the usage but not `turns` is returned anyway,
    so that usage_problems() reports the difference."""
    want = {axis: result.get(axis) for axis in AXES}
    fallback, total, main = None, dict.fromkeys(AXES, 0), 0
    for index in range(start, len(exchanges)):
        exchange = exchanges[index]
        main += exchange.kind == "main"
        for axis in AXES:
            value = exchange.reply.usage.get(axis) if exchange.reply is not None else None
            if isinstance(value, (int, float)) and not isinstance(value, bool):
                total[axis] += int(value)
        if total == want:
            if "turns" not in result or result["turns"] == main:
                return index + 1
            fallback = fallback or index + 1
    return fallback


def carry_evidence(exchanges, books):
    """What the capture itself says about `preserved_reasoning`: whether the
    wire echoes a reasoning block anywhere, whether a session file stores one."""
    echoed = any(split_reasoning(wire_content(message))[0]
                 for exchange in exchanges
                 for message in exchange.body.get("messages") or []
                 if isinstance(message, dict) and message.get("role") == "assistant")
    stored = any(isinstance(message, dict) and "reasoning" in message
                 for _, book in books for message in book["messages"])
    return echoed, stored


def cmd_audit(args):
    rep = Reporter(verbose=args.verbose, only=args.only, strict=args.strict)
    started = time.time()
    directory = args.capture_dir
    if read_jsonl(os.path.join(directory, "requests.jsonl")) is None:
        rep.emit("audit: capture directory",
                 "%s holds no readable requests.jsonl — record one with the `capture` subcommand first" % directory, "")
        return rep.finish("audit_anthropic_wire", time.time() - started)
    hits = scan_capture(directory)
    rep.emit("audit: leak scan",
             ["%s matches the %s pattern" % (name, " / ".join(kinds)) for name, kinds in sorted(hits.items())],
             "no key pattern in %s" % ", ".join(CAPTURE_FILES))
    exchanges, broken = capture_exchanges(directory)
    rep.emit("audit: capture files", broken, "every line of the two capture files is a record")
    main = sum(1 for exchange in exchanges if exchange.kind == "main")
    summary = sum(1 for exchange in exchanges if exchange.kind == "summary")
    other = sum(1 for exchange in exchanges if exchange.kind == "other")
    complete = sum(1 for exchange in exchanges if exchange.reply is not None and exchange.reply.stored)
    if not rep.emit(
            "audit: traffic",
            [] if main or summary else
            "none of the %d recorded request(s) is a Messages API call (no `messages` array in its body) — this audit "
            "reads Anthropic-protocol traffic only" % len(exchanges),
            "%d request(s) (%d main + %d summary%s), %d complete repl%s reassembled from the recorded frames"
            % (len(exchanges), main, summary, " + %d other, ignored" % other if other else "", complete,
               "y" if complete == 1 else "ies")):
        return rep.finish("audit_anthropic_wire", time.time() - started)

    books = []
    for path in args.session or []:
        book, name = load_json(path), os.path.basename(path)
        if isinstance(book, dict) and isinstance(book.get("messages"), list):
            books.append((name, book))
        else:
            rep.emit("audit: session file %s" % name, "not a session file" if book is not None else "no readable JSON",
                     "a session file")

    # Carry is not on the wire as a flag: read it off the evidence. With none
    # either way the default is checked (on) and the reader is told how to
    # declare otherwise — a run that signed thinking blocks and echoed none is
    # either `preserved_reasoning: false` or a bug, and the checker must not
    # guess the flattering one.
    echoed, stored = carry_evidence(exchanges, books)
    if args.carry_off:
        carry_on, why = False, "off (--carry-off)"
    else:
        carry_on = True
        why = ("on: the wire echoes reasoning blocks" if echoed else
               "on: the session file stores reasoning" if stored else
               "on: nothing on the wire or in a session file says so — pass --carry-off if the run really had "
               "`preserved_reasoning` off")
    print("[note] carry read as %s" % why)

    first = next((exchange for exchange in exchanges if exchange.kind == "main"), None)
    strict = first is not None and len(first.body.get("messages") or []) == 1
    if not strict:
        print("[note] the capture does not start from the conversation's first request: assistant messages whose "
              "replies predate it are held to the wire's own earlier form, not to the endpoint (notes, not failures)")
    audit = audit_traffic(exchanges, carry_on, strict_sources=strict, detail=rep.detail("audit"))
    emit_traffic(rep, "audit", audit, carry_on, quiet="preserved_reasoning off (--carry-off)" if args.carry_off else "")
    unverified = audit["notes"].get("unverified") or []
    if unverified:
        print("[note] audit: unverified — %d assistant message(s) match no recorded reply%s: %s" % (
            len(unverified), "" if rep.verbose else " (--verbose lists them)", "; ".join(unverified) if rep.verbose
            else unverified[0]))
    for name, book in books:
        emit_session(rep, "audit", book, audit, carry_on, note="session %s" % name, strict=strict)

    start = 0
    for index, path in enumerate(args.result or [], 1):
        result, name = load_json(path), os.path.basename(path)
        label = "audit: result.json #%d" % index
        if result is None:
            rep.emit(label, "%s holds no readable JSON" % name, "")
            continue
        if not (isinstance(result, dict) and all(axis in result for axis in AXES)):
            rep.emit(label, "%s is not a result.json (its four usage axes are missing)" % name, "")
            continue
        end = usage_window(result, exchanges, start)
        if end is None:
            rep.emit(label, "no run of the capture's requests from #%s on sums to what %s reports (input %s / output %s "
                             "/ cache write %s / cache read %s) — was this run made through the proxy?"
                     % (exchanges[start].number if start < len(exchanges) else "—", name,
                        result.get("input_tokens"), result.get("output_tokens"),
                        result.get("cache_creation_input_tokens"), result.get("cache_read_input_tokens")), "")
            continue
        window = exchanges[start:end]
        span = "requests #%s–#%s" % (window[0].number, window[-1].number)
        problems = usage_problems(result, window, "result.json #%d (%s)" % (index, name))
        rep.emit("%s (%s, %s)" % (label, name, span), problems,
                 "%s — each axis equals result.json over this run's requests; reasoning_tokens absent; turns = %s"
                 % (usage_text(window), result.get("turns")))
        start = end

    rep.emit("self-test: mutations", self_test(), SELF_TEST_OK, always=True)
    return rep.finish("audit_anthropic_wire", time.time() - started)


# ── entry point ───────────────────────────────────────────────────────────────


def build_parser():
    parser = argparse.ArgumentParser(
        prog="e2e_anthropic_wire.py",
        description="On-the-wire verification of the Anthropic-protocol reasoning carry. "
                    "No subcommand means check.")
    sub = parser.add_subparsers(dest="command")
    check = sub.add_parser("check", help="offline gate: the real binary against an in-process scripted endpoint")
    check.add_argument("--verbose", action="store_true", help="per-request detail, every problem")
    check.add_argument("--only", metavar="SUBSTR", help="only the checks whose name contains SUBSTR")
    check.add_argument("--strict", action="store_true",
                       help="a known kernel gap counts as a failure (same as TA_WIRE_STRICT=1)")
    check.add_argument("--keep", action="store_true", help="keep the scratch directory")
    capture = sub.add_parser("capture", help="manual: a loopback forwarding proxy recording both directions")
    capture.add_argument("--upstream", required=True, help="base URL of the endpoint to forward to, e.g. https://HOST")
    capture.add_argument("--listen", default="127.0.0.1:0", metavar="HOST:PORT",
                         help="loopback address to listen on (default 127.0.0.1:0 — any free port)")
    capture.add_argument("--out", required=True,
                         help="a new or empty directory for requests.jsonl / responses.jsonl / meta.json")
    capture.add_argument("--upstream-timeout", type=float, default=600.0, metavar="SECONDS",
                         help="per upstream socket operation (default 600)")
    audit = sub.add_parser("audit", help="offline: the same checks over a capture directory")
    audit.add_argument("capture_dir")
    audit.add_argument("--session", action="append", default=[], metavar="FILE", help="session file (repeatable)")
    audit.add_argument("--result", action="append", default=[], metavar="FILE", help="result.json (repeatable)")
    audit.add_argument("--carry-off", action="store_true", help="the audited run had `preserved_reasoning` off")
    audit.add_argument("--verbose", action="store_true", help="per-request detail, every problem")
    audit.add_argument("--only", metavar="SUBSTR", help="only the checks whose name contains SUBSTR")
    audit.add_argument("--strict", action="store_true",
                       help="a known kernel gap counts as a failure (same as TA_WIRE_STRICT=1)")
    return parser


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    # No subcommand means check — except a bare --help: it must introduce all
    # three subcommands, not just the default one.
    if not argv or (argv[0].startswith("-") and argv[0] not in ("-h", "--help")):
        argv.insert(0, "check")
    args = build_parser().parse_args(argv)
    return {"check": cmd_check, "capture": cmd_capture, "audit": cmd_audit}[args.command](args)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
