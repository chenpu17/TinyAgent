"""push_user content blocks + the hot-update surface (set_model family, MCP
add/remove). Fully offline: a Python custom provider answers the turns, and
the MCP server is a tiny script written into tmp_path."""
from __future__ import annotations

import sys
import textwrap

import pytest

import tiny_agent

from conftest import openai_config

MCP_SERVER = textwrap.dedent(
    """
    import json
    import sys
    import time

    delay = float(sys.argv[1]) if len(sys.argv) > 1 else 0.0

    def reply(msg_id, result):
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg_id, "result": result}) + "\\n")
        sys.stdout.flush()

    def main():
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            msg = json.loads(line)
            method = msg.get("method")
            if method == "initialize":
                if delay:
                    time.sleep(delay)
                reply(msg["id"], {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "test-mcp", "version": "0.1.0"},
                })
            elif method == "tools/list":
                reply(msg["id"], {"tools": [{
                    "name": "echo",
                    "description": "Echo back the given text.",
                    "inputSchema": {"type": "object",
                                    "properties": {"text": {"type": "string"}},
                                    "required": ["text"]},
                }]})

    main()
    """
)


def write_mcp_server(tmp_path) -> str:
    path = tmp_path / "mcp_echo.py"
    path.write_text(MCP_SERVER)
    return str(path)


def recording_provider(seen: list, text: str = "ok"):
    def provider(request):
        seen.append(request)
        return {"text": text, "stop_reason": "end_turn"}

    return provider


# ── push_user content blocks ────────────────────────────────────────────────

def test_push_user_str_shorthand_still_one_text_block(tmp_path):
    a = tiny_agent.Agent(
        openai_config("http://unused", workdir=str(tmp_path)), api_key="test-key"
    )
    a.push_user("plain text")
    msgs = a.messages()
    assert len(msgs) == 1
    assert msgs[0]["role"] == "user"
    assert msgs[0]["content"] == [{"text": "plain text"}]


def test_push_user_single_image_block(tmp_path):
    a = tiny_agent.Agent(
        openai_config("http://unused", workdir=str(tmp_path)), api_key="test-key"
    )
    a.push_user({"type": "image", "media_type": "image/png", "data": "aGVsbG8="})
    block = a.messages()[0]["content"][0]
    assert block == {"image": {"media_type": "image/png", "data": "aGVsbG8="}}


def test_push_user_sequence_preserves_order_and_mixes_str(tmp_path):
    a = tiny_agent.Agent(
        openai_config("http://unused", workdir=str(tmp_path)), api_key="test-key"
    )
    a.push_user([
        "caption",
        {"type": "document", "media_type": "application/pdf", "data": "JVBERi0="},
    ])
    content = a.messages()[0]["content"]
    assert content == [
        {"text": "caption"},
        {"document": {"media_type": "application/pdf", "data": "JVBERi0="}},
    ]


def test_push_user_block_round_trips_through_messages(tmp_path):
    """A block read back from messages() can be pushed straight back — the
    wire shape is the same serde form, so a GUI can re-send an attachment."""
    a = tiny_agent.Agent(
        openai_config("http://unused", workdir=str(tmp_path)), api_key="test-key"
    )
    a.push_user({"type": "image", "media_type": "image/jpeg", "data": "/9j/4AAQ"})
    block = a.messages()[0]["content"][0]
    b = tiny_agent.Agent(
        openai_config("http://unused", workdir=str(tmp_path)), api_key="test-key"
    )
    b.push_user(block)
    assert b.messages()[0]["content"][0] == block


@pytest.mark.parametrize(
    "bad, match",
    [
        ([], "at least one content block"),
        ([{"type": "tool_use", "id": "x", "name": "t", "input": {}}], "agent-loop internal"),
        ([{"type": "tool_result", "tool_use_id": "x", "content": "r"}], "agent-loop internal"),
        ([{"type": "audio", "data": "x"}], "unknown content block type"),
        ([{"type": "image", "media_type": "image/png"}], 'needs a string "data" field'),
        ([{"nope": 1}], 'needs a string "type" field'),
        (123, "expects a str"),
        # One dict must carry one shape: silently keeping the first would drop
        # the host's other content block.
        ([{"text": "a", "image": {"media_type": "image/png", "data": "x"}}],
         "mixes multiple message shapes"),
        ([{"type": "text", "text": "a", "image": {"media_type": "image/png", "data": "x"}}],
         'also carries the "image" shape key'),
        # Wrong value types get a precise message, not a fall-through shape error.
        ([{"text": 123}], 'needs a string "text" field'),
        ([{"image": "nope"}], 'needs an object "image" field'),
    ],
)
def test_push_user_rejects_bad_input(tmp_path, bad, match):
    a = tiny_agent.Agent(
        openai_config("http://unused", workdir=str(tmp_path)), api_key="test-key"
    )
    with pytest.raises(RuntimeError, match=match):
        a.push_user(bad)
    assert a.messages() == [], "a rejected push must not append anything"


# ── set_model / limits hot updates ──────────────────────────────────────────

def test_hot_update_model_and_limits_reach_next_request(tmp_path):
    seen: list[dict] = []
    a = tiny_agent.Agent(
        openai_config("http://unused", workdir=str(tmp_path)),
        provider=recording_provider(seen),
    )
    assert a.model() == "mock-model"

    a.set_model("other-model")
    assert a.model() == "other-model"
    a.set_reasoning_effort("high")
    a.set_reasoning_effort(None)
    a.set_max_tokens(0)  # clamped to 1
    a.set_max_context_tokens(4000)
    a.set_max_context_tokens(0)  # back to the char heuristic

    a.push_user("hi")
    a.run()
    assert seen[0]["model"] == "other-model"
    assert seen[0]["max_tokens"] == 1


def test_hot_update_numeric_args_are_validated(tmp_path):
    """Negative / fractional / out-of-range budgets are rejected instead of
    being silently truncated or wrapped — the same acceptance set as Node."""
    a = tiny_agent.Agent(
        openai_config("http://unused", workdir=str(tmp_path)), api_key="test-key"
    )

    with pytest.raises(ValueError, match="max_tokens must be an integer in 0"):
        a.set_max_tokens(-5)
    with pytest.raises(ValueError, match="max_tokens must be an integer in 0"):
        a.set_max_tokens(2**40)
    with pytest.raises(TypeError):
        a.set_max_tokens(1.7)

    with pytest.raises(ValueError, match="tokens must be an integer >= 0"):
        a.set_max_context_tokens(-1)
    with pytest.raises(TypeError):
        a.set_max_context_tokens(1.5)
    with pytest.raises(OverflowError):
        a.set_max_context_tokens(2**70)

    # Boundaries stay accepted.
    a.set_max_tokens(0)
    a.set_max_tokens(4294967295)
    a.set_max_context_tokens(0)
    a.set_max_context_tokens(2**63 - 1)


def test_hot_update_from_the_run_thread_errors_instead_of_deadlocking(tmp_path):
    """A provider / tool callback runs on the run thread while the agent lock
    is held; calling a hot-update method there must raise the explanatory
    RuntimeError, not self-deadlock."""
    holder = {}
    errors = []

    def provider(request):
        try:
            holder["agent"].set_model("from-provider")
        except RuntimeError as e:
            errors.append(str(e))
        return {"text": "ok", "stop_reason": "end_turn"}

    a = tiny_agent.Agent(
        openai_config("http://unused", workdir=str(tmp_path)), provider=provider
    )
    holder["agent"] = a
    a.push_user("hi")
    a.run()

    assert errors and "Agent is running on this thread" in errors[0]
    assert a.model() == "mock-model", "the rejected update must not take effect"


# ── MCP hot add / remove ────────────────────────────────────────────────────

def test_mcp_hot_add_remove_changes_next_request_toolset(tmp_path):
    script = write_mcp_server(tmp_path)
    seen: list[dict] = []
    a = tiny_agent.Agent(
        openai_config("http://unused", workdir=str(tmp_path)),
        provider=recording_provider(seen),
    )

    added = a.add_mcp_server({"name": "echo", "command": sys.executable, "args": [script]})
    assert added == ["mcp_echo_echo"]

    a.push_user("first")
    a.run()
    assert "mcp_echo_echo" in [t["name"] for t in seen[0]["tools"]]

    assert a.remove_mcp_server("echo") == 1
    a.push_user("second")
    a.run()
    assert "mcp_echo_echo" not in [t["name"] for t in seen[1]["tools"]]

    with pytest.raises(RuntimeError, match="no MCP server 'echo'"):
        a.remove_mcp_server("echo")


def test_mcp_name_collision_is_recoverable_and_leaves_agent_unchanged(tmp_path):
    script = write_mcp_server(tmp_path)
    seen: list[dict] = []
    a = tiny_agent.Agent(
        openai_config("http://unused", workdir=str(tmp_path)),
        provider=recording_provider(seen),
    )
    a.add_mcp_server({"name": "echo", "command": sys.executable, "args": [script]})
    with pytest.raises(RuntimeError, match="collision"):
        a.add_mcp_server({"name": "echo", "command": sys.executable, "args": [script]})

    a.push_user("hi")
    a.run()
    names = [t["name"] for t in seen[0]["tools"]]
    assert names.count("mcp_echo_echo") == 1


def test_mcp_add_rejects_unknown_keys(tmp_path):
    a = tiny_agent.Agent(
        openai_config("http://unused", workdir=str(tmp_path)), api_key="test-key"
    )
    with pytest.raises(RuntimeError):
        a.add_mcp_server({"name": "x", "command": "true", "bogus": 1})
