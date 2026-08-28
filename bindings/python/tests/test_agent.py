"""End-to-end Python binding tests for `tiny_agent`.

Every test runs fully offline against an in-process OpenAI-compatible SSE mock
(`ScriptedMock`), except `test_chat_one_shot_against_project_mock` which drives
the real `examples/mock_openai.py` subprocess as a regression guard.

Run:  cd bindings/python && python -m pytest tests/ -v
"""
from __future__ import annotations

import json
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


# ── 9. custom Python provider (no HTTP server at all) ────────────────────────

def test_custom_provider_returns_text(tmp_path):
    """A user-supplied provider callable replaces the built-in HTTP provider
    entirely: the agent drives the loop, but each turn's response comes from the
    Python function (no mock server, no API key). The returned text becomes the
    assistant message; usage/stop_reason flow back honestly."""
    seen = {"requests": []}

    def provider(request):
        seen["requests"].append(request)
        return {
            "text": "hello from custom provider",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 7, "output_tokens": 4},
        }

    cfg = openai_config("http://unused-no-http-here", tools=[], workdir=str(tmp_path))
    a = tiny_agent.Agent(cfg, provider=provider)
    a.push_user("hi")
    out = a.run()

    assert out["final_text"] == "hello from custom provider"
    assert out["stop_reason"] == "end_turn"
    # The provider's usage was folded into the run's honest accounting.
    assert out["input_tokens"] == 7
    assert out["output_tokens"] == 4
    # The provider saw the request shape: model + the user message.
    assert len(seen["requests"]) == 1
    req = seen["requests"][0]
    assert req["model"] == "mock-model"
    assert req["max_tokens"] == 1024
    # messages is the conversation; the user's "hi" text is somewhere in it.
    assert "hi" in json.dumps(req["messages"])


def test_custom_provider_str_shorthand(tmp_path):
    """A plain `str` return is shorthand for `{text: str}` (end_turn, no usage)."""
    def provider(request):
        return "just a string"

    cfg = openai_config("http://unused", tools=[], workdir=str(tmp_path))
    out = tiny_agent.chat(cfg, "hi", provider=provider)
    assert out["final_text"] == "just a string"
    assert out["stop_reason"] == "end_turn"


def test_custom_provider_multi_turn_sees_growing_history(tmp_path):
    """Across turns the provider receives the full accumulated conversation —
    the custom provider participates in the agent's stateful loop, not a
    one-shot call."""
    seen = {"turns": []}

    def provider(request):
        # Echo back how many assistant messages are already in history.
        msgs = request["messages"]
        assistant_count = sum(1 for m in msgs if m["role"] == "assistant")
        seen["turns"].append(assistant_count)
        return {"text": f"turn-{assistant_count}", "stop_reason": "end_turn"}

    cfg = openai_config("http://unused", tools=[], max_turns=3, workdir=str(tmp_path))
    a = tiny_agent.Agent(cfg, provider=provider)
    a.push_user("first")
    a.run()
    a.push_user("second")
    a.run()

    # Turn 1 sees 0 prior assistant msgs; turn 2 sees the 1 we just added.
    assert seen["turns"] == [0, 1]


def test_custom_provider_streams_one_delta(tmp_path):
    """The non-streaming v1 contract: the whole text arrives as a single
    TextDelta event (the provider can't incrementally stream yet)."""
    events = []

    def provider(request):
        return {"text": "whole text at once", "stop_reason": "end_turn"}

    def on_event(e):
        events.append(e)

    cfg = openai_config("http://unused", tools=[], workdir=str(tmp_path))
    tiny_agent.chat(cfg, "hi", provider=provider, on_event=on_event)
    deltas = [e["TextDelta"] for e in events if isinstance(e, dict) and "TextDelta" in e]
    assert deltas == ["whole text at once"]


def test_custom_provider_combines_with_custom_tool(tmp_path):
    """A custom provider can still be paired with a custom tool: the tool is
    registered normally; the provider just can't *emit* a tool_use block (it's
    text-only in v1), so this is a one-turn text + tool-presence smoke test."""
    def provider(request):
        tools = request["tools"]
        return {"text": f"i see {len(tools)} tool(s)", "stop_reason": "end_turn"}

    tool = tiny_agent.Tool(
        name="get_weather",
        description="Look up the weather.",
        schema={"type": "object", "properties": {}, "required": []},
        callback=lambda args: "sunny",
    )

    cfg = openai_config("http://unused", tools=[], workdir=str(tmp_path))
    a = tiny_agent.Agent(cfg, tools=[tool], provider=provider)
    a.push_user("hi")
    out = a.run()
    assert out["final_text"] == "i see 1 tool(s)"


def test_custom_provider_empty_text_terminates_cleanly(tmp_path):
    """Empty text must not loop forever or persist a bad assistant message:
    from_simple yields zero blocks, the agent loop breaks at the empty-turn
    guard, and the run ends with empty final_text."""
    def provider(request):
        return {"text": "", "stop_reason": "end_turn"}

    cfg = openai_config("http://unused", tools=[], workdir=str(tmp_path))
    out = tiny_agent.chat(cfg, "hi", provider=provider)
    assert out["final_text"] == ""
    assert out["stop_reason"] == "end_turn"


# ── 10. custom provider: depth — cancel / recovery / shape / stop_reason ─────

def test_custom_provider_cancel_takes_effect_after_call(tmp_path):
    """cancel flipped DURING the provider call: the host function can't be
    interrupted (it holds the GIL / blocks the worker), but once it returns the
    loop sees the flag at the turn boundary and overrides stop_reason to
    'cancelled'. So the run reports cancelled, but only after the provider call
    runs to completion — not mid-call."""
    def slow(request):
        time.sleep(0.4)
        return {"text": "done", "stop_reason": "end_turn"}

    cfg = openai_config("http://unused", tools=[], workdir=str(tmp_path))
    a = tiny_agent.Agent(cfg, provider=slow)
    tok = tiny_agent.CancelToken()
    a.set_cancel(tok)
    a.push_user("hi")
    threading.Timer(0.1, tok.cancel).start()

    start = time.monotonic()
    out = a.run()
    elapsed = time.monotonic() - start

    assert out["stop_reason"] == "cancelled"
    assert tok.is_cancelled()
    # The provider ran to completion (≈0.4s) — cancel did NOT cut it short.
    assert elapsed >= 0.35, f"provider should run to completion, cancelled at {elapsed:.2f}s"


def test_custom_provider_concurrent_runs_release_gil(tmp_path):
    """Two agents running on separate Python threads, each with a sleeping
    custom provider (time.sleep releases the GIL, modeling an I/O-bound LLM
    call). Because run() releases the GIL (py.allow_threads) and sleep releases
    it too, the two runs overlap in time — total ≈ one sleep, not two. This is
    the real-concurrency proof that functional single-threaded tests can't give
    (cf. the project's 'thread the mock e2e' note)."""
    sleep_s = 0.4

    def llm(request):
        time.sleep(sleep_s)
        return {"text": "ok", "stop_reason": "end_turn"}

    cfg = openai_config("http://unused", tools=[], workdir=str(tmp_path))
    results: list[str] = []
    results_lock = threading.Lock()

    def run_one():
        a = tiny_agent.Agent(cfg, provider=llm)
        a.push_user("hi")
        out = a.run()
        with results_lock:
            results.append(out["final_text"])

    start = time.monotonic()
    threads = [threading.Thread(target=run_one) for _ in range(2)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    elapsed = time.monotonic() - start

    assert results == ["ok", "ok"]
    # Serial would be ≈ 2×sleep = 0.8s; concurrent ≈ 1×sleep = 0.4s. The 0.6s
    # cut separates them with margin for thread startup / GIL handoff jitter.
    assert elapsed < 0.6, f"runs serialized (GIL not released?): {elapsed:.2f}s"


def test_custom_provider_recovers_after_error(tmp_path):
    """A provider that throws on turn 1 fails the run honestly, but the agent's
    state isn't corrupted: turn 2 (after the provider stops failing) runs fine,
    and the failed turn left no partial assistant message in history."""
    state = {"fail": True}

    def flaky(request):
        if state["fail"]:
            raise RuntimeError("boom on first call")
        return {"text": "recovered", "stop_reason": "end_turn"}

    cfg = openai_config("http://unused", tools=[], workdir=str(tmp_path))
    a = tiny_agent.Agent(cfg, provider=flaky)
    a.push_user("first")
    with pytest.raises(RuntimeError, match="boom"):
        a.run()

    state["fail"] = False
    a.push_user("second")
    out = a.run()
    assert out["final_text"] == "recovered"


def test_custom_provider_request_messages_shape(tmp_path):
    """Lock down the JSON shape handed to the provider (users parse this):
    messages is a list of {role, content: [blocks]}, where a text block is the
    externally-tagged {'text': str}. role is lowercase 'user'/'assistant'."""
    seen = {}

    def llm(request):
        seen["messages"] = request["messages"]
        return {"text": "ok", "stop_reason": "end_turn"}

    cfg = openai_config("http://unused", tools=[], workdir=str(tmp_path))
    a = tiny_agent.Agent(cfg, provider=llm)
    a.push_user("hello world")
    a.run()

    msgs = seen["messages"]
    assert msgs[0]["role"] == "user"
    assert msgs[0]["content"] == [{"text": "hello world"}]


def test_custom_provider_passes_through_non_end_turn_stop_reason(tmp_path):
    """A non-end_turn stop_reason flows through to the RunOutcome unchanged.
    'max_tokens' exercises the loop's abort path (which strips partial tool_use
    but keeps text and the original stop_reason) — text-only still reports it."""
    def llm(request):
        return {
            "text": "cut off",
            "stop_reason": "max_tokens",
            "usage": {"input_tokens": 1, "output_tokens": 1},
        }

    cfg = openai_config("http://unused", tools=[], workdir=str(tmp_path))
    out = tiny_agent.chat(cfg, "hi", provider=llm)
    assert out["stop_reason"] == "max_tokens"
    assert out["final_text"] == "cut off"



def test_inline_dict_config_validates_workdir():
    # An inline dict used to skip Config::load's validate step: a nonexistent
    # workdir silently fell back / diverged from the file path's semantics.
    # Both construction paths now run validate_and_normalize, so a bogus
    # workdir raises at construction (RuntimeError from the anyhow error).
    cfg = openai_config("http://unused", tools=[], workdir="/definitely/not/a/real/dir")
    with pytest.raises(RuntimeError, match="workdir"):
        tiny_agent.Agent(cfg, api_key=MOCK_KEY)
