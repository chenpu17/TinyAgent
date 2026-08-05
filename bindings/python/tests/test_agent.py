"""End-to-end Python binding tests for `tiny_agent`.

Every test runs fully offline against an in-process OpenAI-compatible SSE mock
(`ScriptedMock`), except `test_chat_one_shot_against_project_mock` which drives
the real `examples/mock_openai.py` subprocess as a regression guard.

Run:  cd bindings/python && python -m pytest tests/ -v
"""
from __future__ import annotations

import threading
import time

import pytest

import tiny_agent

from conftest import (  # type: ignore[import-not-found]
    ProjectMock,
    ScriptedMock,
    openai_config,
    text_sse,
    tool_call_sse,
    write_sse,
)

MOCK_KEY = "mock-key"  # the binding takes the key explicitly; never read from env


# ── 1. one-shot chat against the real project mock asset ─────────────────────

def test_chat_one_shot_against_project_mock(tmp_path):
    """`chat()` drives a full tool round-trip against examples/mock_openai.py:
    turn 1 → write_file tool_call, turn 2 → final text. Proves the binding works
    against the project's own mock, and that a built-in tool's side effect lands
    in the configured workdir."""
    mock = ProjectMock()
    try:
        cfg = openai_config(mock.url, workdir=str(tmp_path))
        out = tiny_agent.chat(cfg, "write the mock file", api_key=MOCK_KEY)

        assert "Task complete" in out["final_text"]
        assert "mock_output.txt" in out["final_text"]
        assert out["tool_calls"], "tool_calls should be recorded"
        assert out["tool_calls"][0]["name"] == "write_file"
        # The side effect landed in the workdir (resolve() joins relative paths
        # onto ctx.workdir).
        assert (tmp_path / "mock_output.txt").read_text() == "mock ok\n"
        assert out["turns"] >= 2
    finally:
        mock.stop()


# ── 2. a user-defined Python tool is actually invoked ────────────────────────

def test_custom_python_tool_invoked(tmp_path):
    """The headline capability: a Python callable registered as a Tool is
    dispatched by the agent, its return value flows back to the model, and the
    call is recorded in tool_calls with honest input/output."""
    seen = {"calls": []}

    def get_weather(args):
        seen["calls"].append(args)
        return {"content": "sunny, 72F"}  # dict form → content + is_error=False

    tool = tiny_agent.Tool(
        name="get_weather",
        description="Look up the weather for a city.",
        schema={
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
        },
        callback=get_weather,
    )

    def first(handler, body):
        write_sse(handler, tool_call_sse("c1", "get_weather", {"city": "Boston"}))

    def second(handler, body):
        write_sse(handler, text_sse("The weather in Boston is sunny."))

    mock = ScriptedMock([first, second]).start()
    try:
        cfg = openai_config(mock.url, tools=[], workdir=str(tmp_path))
        a = tiny_agent.Agent(cfg, tools=[tool], api_key=MOCK_KEY)
        a.push_user("weather in Boston?")
        out = a.run()

        # The Python callback ran exactly once, with the model's parsed args.
        assert len(seen["calls"]) == 1
        assert seen["calls"][0]["city"] == "Boston"

        # The call is recorded with its input and the tool's textual output.
        assert out["tool_calls"][0]["name"] == "get_weather"
        assert out["tool_calls"][0]["input"]["city"] == "Boston"
        assert "sunny" in out["tool_calls"][0]["output"]
        assert out["tool_calls"][0]["is_error"] is False

        assert "sunny" in out["final_text"]
    finally:
        mock.stop()


def test_custom_tool_accepts_str_return(tmp_path):
    """A plain `str` return is shorthand for `{content: str}`."""
    def echo(args):
        return f"echo:{args['msg']}"

    tool = tiny_agent.Tool("echo", "echo back", {"type": "object",
                            "properties": {"msg": {"type": "string"}},
                            "required": ["msg"]}, echo)

    def first(handler, body):
        write_sse(handler, tool_call_sse("c1", "echo", {"msg": "hi"}))

    def second(handler, body):
        write_sse(handler, text_sse("done"))

    mock = ScriptedMock([first, second]).start()
    try:
        a = tiny_agent.Agent(openai_config(mock.url, tools=[], workdir=str(tmp_path)),
                             tools=[tool], api_key=MOCK_KEY)
        a.push_user("echo hi")
        out = a.run()
        assert out["tool_calls"][0]["output"] == "echo:hi"
    finally:
        mock.stop()


# ── 3. multi-turn Agent + getters ────────────────────────────────────────────

def test_agent_getters_reflect_state(tmp_path):
    """model()/usage()/messages() surface the agent's internal state to Python."""
    def first(handler, body):
        write_sse(handler, tool_call_sse("c1", "read_file", {"path": "x"}))
    # read_file on a nonexistent file → is_error; the agent still continues.

    def second(handler, body):
        write_sse(handler, text_sse("all done"))

    mock = ScriptedMock([first, second]).start()
    try:
        cfg = openai_config(mock.url, tools=["read_file"], workdir=str(tmp_path))
        a = tiny_agent.Agent(cfg, api_key=MOCK_KEY)
        assert a.model() == "mock-model"

        a.push_user("read x")
        out = a.run()

        assert out["stop_reason"] == "end_turn"  # OpenAI "stop" → "end_turn"
        # messages() returns the conversation history as a list of dicts.
        msgs = a.messages()
        assert isinstance(msgs, list)
        assert len(msgs) >= 2  # at least the user turn + one assistant turn
        # usage() returns the 4-axis dict, summed across the run.
        u = a.usage()
        for k in ("input_tokens", "output_tokens",
                  "cache_creation_input_tokens", "cache_read_input_tokens"):
            assert k in u
    finally:
        mock.stop()


# ── 4. cancellation ──────────────────────────────────────────────────────────

def test_cancel_interrupts_run():
    """A CancelToken flipped from another thread interrupts the in-flight turn;
    run() returns (it does not hang) with stop_reason == 'cancelled'. The GIL is
    released during run() (py.allow_threads), so the cancel thread can run."""
    hang_stop = threading.Event()

    def hang(handler, body):
        # Start the stream (so CancellableSse is active), then hold it open
        # without sending data: Content-Length promises more bytes than we
        # write, so the reader blocks mid-stream. The agent's cancel poll
        # (≤200ms) notices the flag and aborts long before idle/total timeouts.
        handler.send_response(200)
        handler.send_header("Content-Type", "text/event-stream")
        handler.send_header("Content-Length", "9999")
        handler.end_headers()
        handler.wfile.write(b": keep-alive\n\n")  # SSE comment, no data frame
        handler.wfile.flush()
        hang_stop.wait(30.0)

    mock = ScriptedMock([hang]).start()
    try:
        cfg = openai_config(mock.url, tools=[], stream_idle_secs=30)
        a = tiny_agent.Agent(cfg, api_key=MOCK_KEY)
        a.push_user("hi")
        tok = tiny_agent.CancelToken()
        a.set_cancel(tok)

        threading.Thread(
            target=lambda: (time.sleep(0.3), tok.cancel()), daemon=True
        ).start()

        out = a.run()
        assert tok.is_cancelled()
        assert out["stop_reason"] == "cancelled"
    finally:
        mock.stop()
        hang_stop.set()  # release the blocked handler thread


# ── 5. honest 4-axis token usage ──────────────────────────────────────────────

def test_usage_four_axes_reported():
    """Usage reported by the provider reaches Python with all four axes.
    OpenAI's mapping: input = prompt_tokens − cached_tokens, cache_read =
    cached_tokens, cache_creation = 0 (OpenAI has no separate cache-write
    event). Verifies the binding round-trips the Usage struct faithfully."""
    usage = {
        "prompt_tokens": 42,
        "completion_tokens": 9,
        "total_tokens": 51,
        "prompt_tokens_details": {"cached_tokens": 30},
    }

    def respond(handler, body):
        write_sse(handler, text_sse("done", usage=usage))

    mock = ScriptedMock([respond]).start()
    try:
        out = tiny_agent.chat(openai_config(mock.url, tools=[]), "hi", api_key=MOCK_KEY)
        assert out["input_tokens"] == 12               # 42 − 30 cached
        assert out["output_tokens"] == 9
        assert out["cache_read_input_tokens"] == 30
        assert out["cache_creation_input_tokens"] == 0  # OpenAI: no creation axis
    finally:
        mock.stop()


# ── 6. streaming events reach the callback ───────────────────────────────────

def test_events_stream_to_callback():
    """on_event receives AgentEvents as dicts (externally-tagged enum shape,
    e.g. {'TextDelta': '...'}) as they happen, not only the final outcome."""
    events = []

    def respond(handler, body):
        write_sse(handler, text_sse("streamed answer"))

    mock = ScriptedMock([respond]).start()
    try:
        tiny_agent.chat(openai_config(mock.url, tools=[]), "hi",
                        on_event=lambda e: events.append(e), api_key=MOCK_KEY)
        # At least one text delta and a terminal Done event were forwarded.
        assert any(isinstance(e, dict) and "TextDelta" in e for e in events), events
        assert any(isinstance(e, dict) and "Done" in e for e in events), events
    finally:
        mock.stop()
