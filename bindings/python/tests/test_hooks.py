"""Hooks interception layer exposed to Python (set_hooks / hooks= kwarg).

Covers the full decision surface offline against ScriptedMock:
  - Deny: the call never executes, the model sees the deny message as the
    error tool_result, the run continues to completion
  - Modify: the REPLACED input executes while the audit keeps the ORIGINAL
    (core contract — log answers "what did the model ask for")
  - observe: after_tool receives (tool, original input, output view,
    executed, ctx) with ctx attribution (workdir / subagent)
  - on_chat_request: one call per outgoing main-turn request
  - fail-closed: a raising / ill-formed before_tool DENIES (never silently
    allows); observe-only hooks swallow host exceptions
  - str shortcut + {"deny": …} / {"modify": …} dict forms
  - chat() one-shot entry point accepts hooks too
"""
import pytest

import tiny_agent
from conftest import ScriptedMock, openai_config, text_sse, tool_call_sse, write_sse

MOCK_KEY = "mock-key"  # never read — the binding takes the key explicitly


class Hooks:
    """Configurable duck-typed hooks object; each leg constructs one."""

    def __init__(self, deny=None, modify=None, raise_before=False,
                 bad_return=False, str_deny=False):
        self.deny = deny
        self.modify = modify
        self.raise_before = raise_before
        self.bad_return = bad_return
        self.str_deny = str_deny
        self.before = []      # (tool, input, ctx)
        self.after = []       # (tool, input, output, executed, ctx)
        self.requests = []

    def before_tool(self, tool, input, ctx):
        self.before.append((tool, input, ctx))
        if self.raise_before:
            raise RuntimeError("host policy exploded")
        if self.bad_return:
            return 12345  # ill-formed → fail-closed
        if self.deny is not None:
            return {"deny": self.deny} if not self.str_deny else self.deny
        if self.modify is not None:
            return {"modify": self.modify}
        return None

    def after_tool(self, tool, input, output, executed, ctx):
        self.after.append((tool, input, output, executed, ctx))

    def on_chat_request(self, request):
        self.requests.append(request)


def shell_round(*, command):
    """Two-request script: shell tool_call then a final text."""
    def first(handler, body):
        write_sse(handler, tool_call_sse("c1", "shell", {"command": command}))

    def second(handler, body):
        write_sse(handler, text_sse("ok done"))

    return [first, second]


def build_agent(mock, tmp_path, hooks):
    cfg = openai_config(mock.url, tools=["shell"], workdir=str(tmp_path))
    a = tiny_agent.Agent(cfg, api_key=MOCK_KEY)
    a.set_hooks(hooks)
    a.push_user("run the command")
    return a


def test_hooks_deny(tmp_path):
    hooks = Hooks(deny="shell is not allowed here")
    mock = ScriptedMock(shell_round(command="echo original")).start()
    try:
        a = build_agent(mock, tmp_path, hooks)
        out = a.run()
    finally:
        mock.stop()

    # Gate saw the call with attribution ctx (top-level → subagent None).
    assert hooks.before[0][0] == "shell"
    assert hooks.before[0][1] == {"command": "echo original"}
    assert hooks.before[0][2]["subagent"] is None
    assert hooks.before[0][2]["workdir"] == str(tmp_path)

    # Denied call recorded is_error with the deny message as the tool_result…
    rec = out["tool_calls"][0]
    assert rec["is_error"] is True
    assert "shell is not allowed here" in rec["output"]
    # …the command never ran (its would-be output is absent).
    assert "original" not in rec["output"]
    # after_tool reported executed=False for the deny.
    assert hooks.after[0][3] is False
    # The run itself completed normally on the follow-up turn.
    assert out["final_text"] == "ok done"
    assert out["stop_reason"] == "end_turn"


def test_hooks_modify_replaces_input_keeps_original_audit(tmp_path):
    hooks = Hooks(modify={"command": "echo replaced-input"})
    mock = ScriptedMock(shell_round(command="echo original")).start()
    try:
        a = build_agent(mock, tmp_path, hooks)
        out = a.run()
    finally:
        mock.stop()

    # Executed call saw the REPLACEMENT (the shell output proves it)…
    rec = out["tool_calls"][0]
    assert "replaced-input" in rec["output"]
    assert "original" not in rec["output"]
    # …while the audit record keeps the model's ORIGINAL input.
    assert rec["input"] == {"command": "echo original"}
    assert rec["is_error"] is False
    assert hooks.after[0][3] is True


def test_hooks_after_tool_observes(tmp_path):
    hooks = Hooks()
    mock = ScriptedMock(shell_round(command="echo observed")).start()
    try:
        a = build_agent(mock, tmp_path, hooks)
        out = a.run()
    finally:
        mock.stop()

    tool, input, output, executed, ctx = hooks.after[0]
    assert tool == "shell"
    assert input == {"command": "echo observed"}  # original input even when allowed
    assert "observed" in output["content"]  # shell wraps output + exit-code suffix
    assert output["is_error"] is False
    assert executed is True
    assert ctx["subagent"] is None


def test_hooks_before_tool_exception_fails_closed(tmp_path):
    hooks = Hooks(raise_before=True)
    mock = ScriptedMock(shell_round(command="echo boom")).start()
    try:
        a = build_agent(mock, tmp_path, hooks)
        out = a.run()
    finally:
        mock.stop()

    rec = out["tool_calls"][0]
    # A misbehaving gate DENIES — never silently degrades to allow-everything.
    assert rec["is_error"] is True
    assert "hook raised" in rec["output"]
    assert "boom" not in rec["output"]


def test_hooks_bad_return_fails_closed(tmp_path):
    hooks = Hooks(bad_return=True)
    mock = ScriptedMock(shell_round(command="echo bad")).start()
    try:
        a = build_agent(mock, tmp_path, hooks)
        out = a.run()
    finally:
        mock.stop()

    rec = out["tool_calls"][0]
    assert rec["is_error"] is True
    assert "before_tool('shell') refused" in rec["output"]


def test_hooks_str_return_denies(tmp_path):
    hooks = Hooks(deny="nope", str_deny=True)
    mock = ScriptedMock(shell_round(command="echo x")).start()
    try:
        a = build_agent(mock, tmp_path, hooks)
        out = a.run()
    finally:
        mock.stop()
    assert out["tool_calls"][0]["is_error"] is True
    assert "nope" in out["tool_calls"][0]["output"]


def test_hooks_on_chat_request(tmp_path):
    hooks = Hooks()
    mock = ScriptedMock(shell_round(command="echo req")).start()
    try:
        a = build_agent(mock, tmp_path, hooks)
        a.run()
    finally:
        mock.stop()

    # Two main-turn requests (tool round + final text), each observed with
    # the host-relevant projection (no cancel/timeout plumbing).
    assert len(hooks.requests) == 2
    for req in hooks.requests:
        assert req["model"] == "mock-model"
        assert req["max_tokens"] == 1024
        assert isinstance(req["messages"], list)
        assert any(t["name"] == "shell" for t in req["tools"])


def test_chat_one_shot_accepts_hooks(tmp_path):
    hooks = Hooks(deny="denied by one-shot policy")
    mock = ScriptedMock(shell_round(command="echo one-shot")).start()
    try:
        cfg = openai_config(mock.url, tools=["shell"], workdir=str(tmp_path))
        out = tiny_agent.chat(cfg, "run it", api_key=MOCK_KEY, hooks=hooks)
    finally:
        mock.stop()

    assert out["tool_calls"][0]["is_error"] is True
    assert "denied by one-shot policy" in out["tool_calls"][0]["output"]


def test_hooks_object_without_methods_is_noop(tmp_path):
    """An object with none of the hook methods installs cleanly and allows all."""
    mock = ScriptedMock(shell_round(command="echo passthrough")).start()
    try:
        cfg = openai_config(mock.url, tools=["shell"], workdir=str(tmp_path))
        out = tiny_agent.chat(cfg, "go", api_key=MOCK_KEY, hooks=object())
    finally:
        mock.stop()

    assert out["tool_calls"][0]["is_error"] is False
    assert "passthrough" in out["tool_calls"][0]["output"]


def test_set_hooks_rejects_broken_getattr(tmp_path):
    """A hooks object whose attribute access blows up surfaces at set_hooks,
    not as a silent no-op mid-run."""
    class Broken:
        def __getattr__(self, name):
            raise ValueError("broken attribute protocol")

    cfg = openai_config("http://127.0.0.1:1", tools=[], workdir=str(tmp_path))
    a = tiny_agent.Agent(cfg, api_key=MOCK_KEY)
    with pytest.raises(ValueError):
        a.set_hooks(Broken())
