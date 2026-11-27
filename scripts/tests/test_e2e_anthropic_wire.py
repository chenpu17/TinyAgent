#!/usr/bin/env python3
"""scripts/e2e_anthropic_wire.py 的本地 pytest（不进 CI，与 test_regress_eval.py 同一地位）。

钉住的是纯逻辑，以及进程内能跑通的全链路：
  * SSE 分帧与重组：任意 chunk 边界、CRLF / 注释 / 多行 data、截断与 error 流「不算完整」；
  * 比较函数：签名、正文、位置、整块丢失、工具入参五种篡改，在线上与会话文件两侧都必须被抓到；
    键序与 1 vs 1.0 两种无害的重编码不得被冤枉；
  * 形状规则：角色交替、tool_use ↔ tool_result N:N、思考块只在运行中那一轮的 assistant 消息里；
    lifetime / carry / echo / source 四条规则各归各位；
  * 压缩折叠后的形状、usage 四轴求和与 result.json 的对窗；
  * capture 的凭据卫生：记录构造器、转发 header 的取舍、被拒的地址与上游从不回显；
  * capture 代理本身（上游一律是本文件自己起的 mock，从不碰真实端点）：流式不缓冲、客户端中途走人、
    上游不可达、chunked 请求体、真实子命令进程的退出码；
  * 回环 server 绑定时不做反向解析（socket.getfqdn 换成直接抛错，mock 与代理照常起）；
  * audit 复用同一套比较函数：篡改过的 capture 必须红；
  * 真实二进制存在时（否则 skip）：穿过代理跑一遍再 audit，整场 check 的端到端，临时目录的清理与 --keep；
  * 已知内核缺口那张表（KNOWN_GAPS）：此刻是空的；登记一条合成的缺口时它以 [GAP] 报告、不计失败，--strict
    致命，登记了却不再复现的缺口让 check 红（修掉的 bug 不许留在表里）；
  * 命令行契约（另起一个解释器，CI 就这么调）：没有二进制是 exit 1 而不是 skip；
  * 接线漂移守卫：check.sh 的 e2e 组与 ci.yml 的 e2e job 在同一处各有一条命令，两份 README 平行；
  * 卫生守卫：脚本与本文件里没有密钥样字面量，没有回环以外的地址。

运行：PYTHONDONTWRITEBYTECODE=1 python3 -m pytest scripts/tests/test_e2e_anthropic_wire.py -q
"""

import codecs
import contextlib
import http.client
import io
import json
import os
import re
import selectors
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from copy import deepcopy
from pathlib import Path
from urllib.parse import urlsplit

import pytest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import e2e_anthropic_wire as wire  # noqa: E402

SCRIPT = ROOT / "scripts" / "e2e_anthropic_wire.py"

# 假凭据一律在运行时拼出来：本文件里不出现任何密钥样字面量（文末的卫生守卫测试会查）。
FAKE_KEY = "dummy-key-" + "V" * 24
FAKE_BEARER = "Bearer " + "raw-bearer-" + "B" * 24
BODY_KEY = "sk-" + "A" * 30
CLIENT_HEADERS = {
    "x-api-key": FAKE_KEY,
    "Authorization": FAKE_BEARER,
    "anthropic-version": wire.ANTHROPIC_VERSION,
    "anthropic-beta": "interleaved-thinking-2025-05-14",
    "content-type": "application/json",
    "accept": "text/event-stream",
    "accept-encoding": "gzip",
    "connection": "keep-alive, X-Drop-Me",
    "x-drop-me": "hop by hop",
    "x-keep-me": "end to end",
}
TOOL = {"name": "write_file", "description": "writes a file", "input_schema": {"type": "object"}}
SIGNATURE = wire.signature("unit-r1")
NEEDS_BINARY = pytest.mark.skipif(
    not (os.path.isfile(wire.BIN) and os.access(wire.BIN, os.X_OK)),
    reason="needs the headless binary (cargo build --locked --features code, or TA_BIN)")


# ── fixtures: a conversation in every shape the checker reads ─────────────────


def opening(text="go"):
    return {"role": "user", "content": [{"type": "text", "text": text}]}


def assistant(content):
    return {"role": "assistant", "content": content}


def tool_result(call_id, content="ok"):
    return {"role": "user", "content": [{"type": "tool_result", "tool_use_id": call_id, "content": content}]}


def exchange(number, messages, reply):
    """One main-loop request and the reply it got, as audit_traffic() reads them."""
    return wire.Exchange(number, {"messages": messages, "tools": [TOOL]}, wire.reply_events(reply, number))


def rules_of(problems):
    return {rule for rule, _ in problems}


def tagged(blocks):
    """Wire content blocks in the session file's externally tagged shape."""
    out = []
    for block in blocks:
        if block["type"] == "text":
            out.append({"text": block["text"]})
        elif block["type"] == "tool_use":
            out.append({"tool_use": {"id": block["id"], "name": block["name"], "input": deepcopy(block["input"])}})
    return out


def side_field(reply):
    """`Message.reasoning` the way the kernel stores a reply's reasoning."""
    return {"provider": "anthropic", "blocks": [
        {"before": position, "text": block["thinking"], "opaque": {"signature": block["signature"]}}
        for position, block in reply.reasoning]}


def conversation():
    """One tool-call round: the endpoint's two replies, the two requests it got, their audit."""
    reply = wire.Reply([wire.thinking(wire.THINKING[0], SIGNATURE), wire.text("Writing it."),
                        wire.tool_use("write_file", {"path": "a.txt", "content": "x\ny", "n": 1}, "toolu_unit_1")],
                       "tool_use", wire.usage_for(1))
    final = wire.Reply([wire.text("Done.")], "end_turn", wire.usage_for(2))
    echoed = [opening(), assistant(wire.splice(reply.reasoning, reply.content)),
              tool_result("toolu_unit_1", "wrote a.txt")]
    audit = wire.audit_traffic([exchange(1, [opening()], reply), exchange(2, echoed, final)], True, True)
    return reply, final, echoed, audit


def session_file(reply, final):
    """The session file the kernel writes for conversation()."""
    return {"version": 1, "model": "mock-claude", "messages": [
        dict(opening(), id="m1"),
        {"id": "m2", "role": "assistant", "content": tagged(reply.content), "reasoning": side_field(reply)},
        dict(tool_result("toolu_unit_1", "wrote a.txt"), id="m3"),
        {"id": "m4", "role": "assistant", "content": tagged(final.content), "reasoning": side_field(final)},
    ]}


def with_first(blocks, kind, **changes):
    """A copy of `blocks` whose first block of type `kind` has `changes` applied."""
    blocks = deepcopy(blocks)
    next(block for block in blocks if block.get("type") == kind).update(changes)
    return blocks


def thinking_to_the_end(blocks):
    blocks = deepcopy(blocks)
    index = next(i for i, block in enumerate(blocks) if block.get("type") == "thinking")
    return blocks[:index] + blocks[index + 1:] + [blocks[index]]


def without_reasoning(blocks):
    return [block for block in blocks if block.get("type") not in wire.REASONING_TYPES]


def reordered(blocks):
    return [dict(reversed(list(block.items()))) for block in blocks]


def float_one(blocks):
    blocks = deepcopy(blocks)
    tool = next(block for block in blocks if block.get("type") == "tool_use")
    tool["input"] = dict(tool["input"], n=1.0)
    return blocks


WIRE_MUTATIONS = {
    "signature altered": (lambda blocks: with_first(blocks, "thinking", signature="X" + SIGNATURE[1:]), {"echo"}),
    "text altered": (lambda blocks: with_first(blocks, "thinking", thinking=wire.THINKING[0] + " "), {"echo"}),
    "position moved": (thinking_to_the_end, {"echo"}),
    "block dropped": (without_reasoning, {"echo"}),
    "tool input changed": (lambda blocks: with_first(blocks, "tool_use", input={"path": "other.txt"}), {"tool"}),
    "key order": (reordered, set()),
    "1 vs 1.0": (float_one, set()),
}

STORED_MUTATIONS = {
    "signature altered": (lambda message: message["reasoning"]["blocks"][0]["opaque"].update(
        signature="X" + SIGNATURE[1:]), {"session"}),
    "text altered": (lambda message: message["reasoning"]["blocks"][0].update(text=wire.THINKING[0] + " "),
                     {"session"}),
    "position moved": (lambda message: message["reasoning"]["blocks"][0].update(before=1), {"session"}),
    "block dropped": (lambda message: message.update(reasoning=None), {"session"}),
    "tool input changed": (lambda message: message["content"][1]["tool_use"]["input"].pop("n"), {"session"}),
    "key order": (lambda message: message["reasoning"]["blocks"][0].update(
        opaque=dict(reversed(list(message["reasoning"]["blocks"][0]["opaque"].items())))), set()),
    "1 vs 1.0": (lambda message: message["content"][1]["tool_use"]["input"].update(n=1.0), set()),
}


# ── SSE: framing and reassembly ───────────────────────────────────────────────


def test_sse_framer_reassembles_at_every_chunk_boundary():
    """按 1、2、3、4 字节切（劈开多字节 UTF-8 与行尾）到整条流一刀，分出来的帧都一样。"""
    reply = wire.Reply([wire.thinking(wire.THINKING[0], wire.signature("sse")),
                        wire.text("步骤 ✓ 🚀 done"),
                        wire.tool_use("write_file", {"path": "中文.txt", "content": "a\nb"}, "toolu_sse")])
    events = wire.reply_events(reply, 3)
    payload = wire.sse_text(events).encode("utf-8")
    assert "🚀".encode("utf-8") in payload           # 多字节字符真的在线上，也没被转成 \\uXXXX
    for size in sorted({1, 2, 3, 4, 7, 13, 64, 700, len(payload)}):
        framer = wire.SseFramer()
        decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
        got = []
        for start in range(0, len(payload), size):
            got += framer.feed(decoder.decode(payload[start:start + size]))
        got += framer.feed(decoder.decode(b"", True)) + framer.finish()
        assert [(event, json.loads(data)) for event, data in got] == events, size
    for size in (1, 5, 31):                          # 按字符切（capture 代理拿到的是解码后的文本）
        whole = payload.decode("utf-8")
        framer = wire.SseFramer()
        got = []
        for start in range(0, len(whole), size):
            got += framer.feed(whole[start:start + size])
        assert got + framer.finish() == [
            (event, json.dumps(data, ensure_ascii=False)) for event, data in events]


def test_sse_framer_tolerates_comments_crlf_and_a_frame_without_a_blank_line():
    text = (": keep-alive\r\nevent: message_start\r\ndata: {\"type\":\r\ndata:  \"message_start\"}\r\n\r\n"
            "event: ping\ndata: {\"type\": \"ping\"}")
    assert wire.parse_sse(text) == [
        ("message_start", '{"type":\n "message_start"}'), ("ping", '{"type": "ping"}')]
    assert wire.parse_sse("") == []
    assert wire.parse_sse("event: x") == [("x", "")]


def test_reassemble_reads_blocks_usage_and_stop_reason():
    reply = wire.Reply(
        [wire.redacted(wire.signature("red")), wire.thinking(wire.THINKING[2], wire.signature("s")),
         wire.text("答案"), wire.tool_use("write_file", {"path": "a b/中文.txt", "n": 2}, "toolu_x")],
        "tool_use", {"input_tokens": 11, "output_tokens": 40, "cache_creation_input_tokens": 3,
                     "cache_read_input_tokens": 7})
    sent = wire.reassemble(wire.reply_events(reply, 9))
    assert sent["complete"] is True and sent["error"] is None and sent["stop_reason"] == "tool_use"
    assert sent["blocks"] == reply.blocks                 # 首尾空白、反斜杠、换行逐字节回来
    assert {axis: sent["usage"][axis] for axis in wire.AXES} == reply.usage


def test_reassemble_honours_what_a_content_block_start_already_carries():
    """起始帧里已带的正文 / 签名 / 入参种子与后续增量拼接（线上通常为空，读到什么算什么）。"""
    def frame(**fields):
        return fields["type"], fields

    sent = wire.reassemble([
        frame(type="content_block_start", index=0,
              content_block={"type": "thinking", "thinking": "head", "signature": "sig-"}),
        frame(type="content_block_delta", index=0, delta={"type": "signature_delta", "signature": "tail"}),
        frame(type="content_block_stop", index=0),
        frame(type="content_block_start", index=1, content_block={"type": "text", "text": "Hel"}),
        frame(type="content_block_delta", index=1, delta={"type": "text_delta", "text": "lo"}),
        frame(type="content_block_stop", index=1),
        frame(type="content_block_start", index=2,
              content_block={"type": "tool_use", "id": "t", "name": "n", "input": {"a": 1}}),
        frame(type="content_block_delta", index=2, delta={"type": "input_json_delta", "partial_json": " "}),
        frame(type="content_block_stop", index=2),
    ])
    assert sent["blocks"] == [{"type": "thinking", "thinking": "head", "signature": "sig-tail"},
                              {"type": "text", "text": "Hello"},
                              {"type": "tool_use", "id": "t", "name": "n", "input": {"a": 1}}]


def test_reassemble_never_calls_a_cut_or_failed_stream_complete():
    events = wire.reply_events(wire.Reply([wire.text("cut")]), 4)
    assert wire.reassemble(events[:-1])["complete"] is False          # 没有 message_stop
    failed = events[:-1] + [("error", {"type": "error", "error": {"type": "overloaded_error", "message": "busy"}})]
    sent = wire.reassemble(failed)
    assert sent["complete"] is False and sent["error"] == "busy"
    assert wire.Reply.from_stream(events[:-1]).stored is False
    assert wire.Reply.from_stream(failed).stored is False
    assert wire.Reply.from_stream(events).stored is True
    junk = [("ping", {"type": "ping"}), ("x", "{not json"),
            ("content_block_delta", {"type": "content_block_delta", "index": 9, "delta": {}})]
    assert wire.reassemble(junk)["blocks"] == []


def test_reassemble_usage_takes_max_on_input_axes_and_last_output():
    frames = [
        ("message_start", {"type": "message_start", "message": {"usage": {
            "input_tokens": 10, "cache_creation_input_tokens": 5, "cache_read_input_tokens": 100, "output_tokens": 1}}}),
        ("message_delta", {"type": "message_delta", "delta": {"stop_reason": None},
                           "usage": {"input_tokens": 3, "output_tokens": 30}}),
        ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                           "usage": {"input_tokens": 12, "output_tokens": 64}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    assert wire.reassemble(frames)["usage"] == {
        "input_tokens": 12, "output_tokens": 64, "cache_creation_input_tokens": 5, "cache_read_input_tokens": 100}


# ── the comparisons behind every check ────────────────────────────────────────


def test_the_scripts_own_mutation_self_test_is_sound():
    assert wire.self_test() == []


@pytest.mark.parametrize("label", sorted(WIRE_MUTATIONS))
def test_each_wire_mutation_lands_on_its_rule(label):
    """线上回传被改：五种篡改各报各的规则，两种无害重编码不报。"""
    reply, final, echoed, audit = conversation()
    mutate, expected = WIRE_MUTATIONS[label]
    content = mutate(wire.splice(reply.reasoning, reply.content))
    problems, matched, counts = wire.echo_problems(
        {"messages": [echoed[0], assistant(content), echoed[2]]}, audit["replies"], True, True, "unit")
    assert rules_of(problems) == expected, problems
    assert set(matched) == {0}                     # tool_use id 认回复：消息仍然对得上端点发的那条
    assert counts == {"echoed": 0 if label == "block dropped" else 1, "expected": 1}


@pytest.mark.parametrize("label", sorted(STORED_MUTATIONS))
def test_each_stored_mutation_lands_on_the_session_rule(label):
    """会话文件里存的被改：同样五种必须红，两种无害重编码不红。"""
    reply, final, _, audit = conversation()
    book = session_file(reply, final)
    mutate, expected = STORED_MUTATIONS[label]
    mutate(book["messages"][1])
    problems, _ = wire.session_problems(book, audit, True, "unit", True)
    assert rules_of(problems) == expected, problems


def test_the_session_file_is_held_to_both_sides():
    reply, final, echoed, audit = conversation()
    book = session_file(reply, final)
    problems, stats = wire.session_problems(book, audit, True, "unit", True)
    assert problems == []
    assert stats["messages"] == 2 and stats["blocks"] == 1 and stats["echoed"] == 1 and stats["final"] == 1
    # 线上被改、文件没改：文件这一侧报「与最后一次回传对不上」
    tampered = with_first(wire.splice(reply.reasoning, reply.content), "thinking", signature="X" + SIGNATURE[1:])
    seen = wire.audit_traffic([exchange(1, [opening()], reply),
                               exchange(2, [echoed[0], assistant(tampered), echoed[2]], final)], True, True)
    assert "echo" in seen["rules"]
    assert rules_of(wire.session_problems(book, seen, True, "unit", True)[0]) == {"session"}
    # 文件被改、线上没改
    book["messages"][1]["reasoning"]["blocks"][0]["opaque"]["signature"] = "Z" + SIGNATURE[1:]
    problems, _ = wire.session_problems(book, audit, True, "unit", True)
    assert rules_of(problems) == {"session"} and all("signature differs" in text for _, text in problems)


def test_a_changed_tool_input_is_a_tool_change_not_an_unknown_reply():
    """tool_use id 认回复：入参被改报的是 tool（「input changed」），不是「端点没发过这条」；
    中途开录的 capture（strict_sources 关）同样不许把它降级成备注。"""
    reply, _, echoed, audit = conversation()
    changed = with_first(wire.splice(reply.reasoning, reply.content), "tool_use", input={"path": "other.txt"})
    body = {"messages": [echoed[0], assistant(changed), echoed[2]]}
    for strict in (True, False):
        problems, matched, _ = wire.echo_problems(body, audit["replies"], True, strict, "unit")
        assert rules_of(problems) == {"tool"} and all("input changed" in text for _, text in problems)
        assert set(matched) == {0}


def test_json_equality_ignores_key_order_but_not_type_changes():
    assert wire.json_equal({"a": [1, {"b": 2, "c": 3}]}, {"a": [1.0, {"c": 3, "b": 2}]})
    assert not wire.json_equal(1, True) and not wire.json_equal(1, "1") and not wire.json_equal([1], [1, 1])
    assert not wire.json_equal({"a": None}, {"a": 0}) and wire.json_equal({"a": None}, {"a": None})


def test_wire_content_reads_both_shapes_and_drops_cache_markers():
    shaped = wire.wire_content({"role": "assistant", "content": [
        {"type": "thinking", "thinking": "t", "signature": "s", "cache_control": {"type": "ephemeral"}},
        {"type": "tool_use", "id": "a", "name": "n", "input": {}}, {"type": "bogus"}]})
    assert [block["type"] for block in shaped] == ["thinking", "tool_use", "bogus"]
    assert "cache_control" not in shaped[0]
    file_shaped = wire.wire_content({"role": "user", "content": [
        {"text": "hi"}, {"tool_result": {"tool_use_id": "a", "is_error": True}}, {"unknown": 1}, "junk"]})
    assert [block["type"] for block in file_shaped] == ["text", "tool_result", "unknown"]
    assert file_shaped[0] == {"type": "text", "text": "hi"} and file_shaped[1]["tool_use_id"] == "a"
    assert wire.wire_content({"content": "plain"}) == [{"type": "text", "text": "plain"}]
    assert wire.wire_content({}) == []


def test_split_and_splice_are_inverse_even_with_adjacent_blocks():
    blocks = [{"type": "thinking", "thinking": "a", "signature": "s1"}, {"type": "redacted_thinking", "data": "d"},
              {"type": "text", "text": "x"}, {"type": "thinking", "thinking": "b", "signature": "s2"},
              {"type": "tool_use", "id": "t", "name": "n", "input": {}}]
    reasoning, rest = wire.split_reasoning(blocks)
    assert [position for position, _ in reasoning] == [0, 0, 1]
    assert [block["type"] for block in rest] == ["text", "tool_use"]
    assert wire.splice(reasoning, rest) == blocks
    assert wire.replayable({"type": "thinking", "thinking": "t", "signature": ""}) is False
    assert wire.replayable({"type": "redacted_thinking", "data": None}) is False
    assert wire.replayable({"type": "redacted_thinking", "data": "d"}) is True


# ── shape rules ───────────────────────────────────────────────────────────────


def valid_body():
    """A request that violates no shape rule: one signed tool call answered by its result."""
    content = wire.splice(
        [(0, wire.thinking(wire.THINKING[0], SIGNATURE))],
        [wire.text("Writing it."),
         wire.tool_use("write_file", {"path": "a.txt", "content": "x\ny", "n": 1}, "toolu_unit_1")])
    return {"messages": [opening(), assistant(content), tool_result("toolu_unit_1", "wrote a.txt")], "tools": [TOOL]}


def test_request_problems_accept_a_faithful_request():
    assert wire.request_problems(valid_body(), "unit") == []
    assert wire.request_problems({"messages": []}, "unit") == [("shape", "unit: `messages` is missing or empty")]
    assert wire.request_problems({}, "unit") == [("shape", "unit: `messages` is missing or empty")]


def test_request_problems_name_each_fault():
    def two_calls_one_id(body):
        body["messages"][1]["content"].append(deepcopy(body["messages"][1]["content"][2]))
        body["messages"][2]["content"] *= 2

    cases = {
        "roles must alternate": (lambda body: body["messages"].insert(1, opening("extra")), "shape"),
        "(user) carries a thinking block": (lambda body: body["messages"][2]["content"].insert(
            0, {"type": "thinking", "thinking": "t", "signature": "s"}), "shape"),
        "has an empty signature": (lambda body: body["messages"][1]["content"][0].update(signature=""), "shape"),
        "has an empty text block": (lambda body: body["messages"][1]["content"][1].update(text=""), "shape"),
        "carries key(s) ['extra']": (lambda body: body["messages"][1]["content"][0].update(extra=1), "shape"),
        "is used more than once": (two_calls_one_id, "pairing"),
        "asked ['toolu_unit_1']": (lambda body: body["messages"][2]["content"][0].update(
            tool_use_id="toolu_other"), "pairing"),
        "content is a bare string": (lambda body: body["messages"][0].update(content="go"), "shape"),
        "although its turn is over": (lambda body: body["messages"].extend(
            [assistant([wire.text("final")]), opening("next")]), "lifetime"),
    }
    for needle, (mutate, rule) in cases.items():
        body = valid_body()
        mutate(body)
        problems = wire.request_problems(body, "unit")
        assert len(problems) == 1 and problems[0][0] == rule, (needle, problems)
        assert needle in problems[0][1], (needle, problems)


def test_a_turn_opens_with_a_user_message_that_carries_no_tool_result():
    nudged = {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "r"},
                                          {"type": "text", "text": "continue"}]}
    messages = [opening(), assistant([wire.text("a")]), opening("two"), tool_result("t1"), nudged,
                assistant([wire.text("b")])]
    assert wire.turn_indices(messages) == [1, 1, 2, 2, 2, 2]


def test_lifetime_carry_and_echo_are_three_different_rules():
    reply, final, echoed, audit = conversation()
    replies = audit["replies"]
    spliced = wire.splice(reply.reasoning, reply.content)
    closed = [echoed[0], assistant(spliced), echoed[2], assistant(wire.splice(final.reasoning, final.content)),
              opening("next")]
    # 上一轮的块还在线上：该不该回传是形状规则的 lifetime；回传了，字节仍须是端点的
    assert rules_of(wire.request_problems({"messages": closed}, "unit")) == {"lifetime"}
    problems, _, counts = wire.echo_problems({"messages": closed}, replies, True, True, "unit")
    assert problems == [] and counts == {"echoed": 1, "expected": 0}
    stripped = deepcopy(closed)
    stripped[1]["content"] = without_reasoning(spliced)
    assert wire.request_problems({"messages": stripped}, "unit") == []
    assert wire.echo_problems({"messages": stripped}, replies, True, True, "unit")[0] == []
    stale = deepcopy(closed)
    stale[1]["content"] = with_first(spliced, "thinking", signature="X" + SIGNATURE[1:])
    assert rules_of(wire.echo_problems({"messages": stale}, replies, True, True, "unit")[0]) == {"echo"}
    # preserved_reasoning 关、或请求里 thinking disabled：回传即 carry
    assert rules_of(wire.echo_problems({"messages": echoed}, replies, False, True, "unit")[0]) == {"carry"}
    disabled = {"messages": deepcopy(echoed), "thinking": {"type": "disabled"}}
    assert rules_of(wire.echo_problems(disabled, replies, True, True, "unit")[0]) == {"carry"}
    # 端点从没发过的消息：mock 全程在场是 source，中途开录只是 unverified 备注
    stranger = [echoed[0], assistant([wire.thinking(wire.THINKING[2], wire.signature("stranger")),
                                      wire.text("never sent by anyone")]), opening("next")]
    assert rules_of(wire.echo_problems({"messages": stranger}, replies, True, True, "unit")[0]) == {"source"}
    loose, _, _ = wire.echo_problems({"messages": stranger}, replies, True, False, "unit", forms={})
    assert rules_of(loose) == {"unverified"}


def test_a_mid_session_capture_holds_a_message_to_its_own_earlier_form():
    """回复早于开录点的消息无从对端点：第一次出现记下形状，之后再出现必须逐字节相同。"""
    old = assistant([wire.thinking(wire.THINKING[2], wire.signature("stranger")),
                     wire.tool_use("write_file", {"path": "old.txt"}, "toolu_old")])
    history = [opening("task"), old, tool_result("toolu_old", "wrote old.txt")]
    first, second = (wire.Reply([wire.text("Done.")], "end_turn", wire.usage_for(n)) for n in (1, 2))
    seen = wire.audit_traffic([exchange(1, history, first)], True, strict_sources=False)
    assert seen["rules"] == {} and len(seen["notes"]["unverified"]) == 1
    assert set(seen["wire_forms"]) == {("tool", "toolu_old")}
    again = exchange(2, deepcopy(history), second)
    assert wire.audit_traffic([exchange(1, history, first), again], True, strict_sources=False)["rules"] == {}
    drifted = deepcopy(history)
    drifted[1]["content"][0]["signature"] = "X" + wire.signature("stranger")[1:]
    later = exchange(2, drifted, second)
    seen = wire.audit_traffic([exchange(1, history, first), later], True, strict_sources=False)
    assert set(seen["rules"]) == {"echo"} and "not re-sent as it was in" in seen["rules"]["echo"][0]


# ── compression: the shape of a fold on the wire ──────────────────────────────


def fold_round():
    """A fold as the wire shows it: one tool-call round, the summary request, the request after."""
    first = wire.Reply([wire.thinking(wire.THINKING[3], wire.signature("fold")), wire.text("step"),
                        wire.tool_use("write_file", {"path": "p.txt", "content": "x"}, "toolu_f_1")], "tool_use",
                       wire.usage_for(1))
    summary = wire.Reply([wire.text("Summary of the rounds so far. Receipts:\nstep")], "end_turn", wire.usage_for(2))
    ask = {"messages": [opening("Summary of the rounds so far. Receipts:\nstep")], "system": "summariser prompt"}
    head = opening("The earlier rounds were folded. Summary of the rounds so far. Receipts:\nstep — end of summary.")
    after = wire.Reply([wire.text("Done.")], "end_turn", wire.usage_for(4))
    return [exchange(1, [opening("task")], first),
            wire.Exchange(2, ask, wire.reply_events(summary, 2)),
            exchange(3, [head], after)]


def test_audit_traffic_pins_a_fold():
    exchanges = fold_round()
    audit = wire.audit_traffic(exchanges, True, True)
    assert audit["rules"] == {} and len(audit["folds"]) == 1
    assert audit["folds"][0]["facts"] == {"before": 3, "after": 1, "folded": 2, "kept": 0}
    assert audit["stats"]["summary"] == 1 and audit["stats"]["main"] == 2
    smuggled = deepcopy(exchanges)                  # 折叠后被塞回来的消息：报 summary
    smuggled[2].body["messages"].append(assistant([wire.text("step")]))
    assert "untouched assistant-led tail" in wire.audit_traffic(smuggled, True, True)["rules"]["summary"][0]


def test_fold_problems_pin_the_shape_after_the_summary_request():
    reply = wire.Reply([wire.thinking(wire.THINKING[3], wire.signature("fold")), wire.text("keep"),
                        wire.tool_use("write_file", {"path": "p.txt", "content": "x"}, "toolu_f_1")], "tool_use")
    history = [opening("task"), assistant([wire.text("step one")]), opening("a note"),
               assistant(wire.splice(reply.reasoning, reply.content)), tool_result("toolu_f_1", "wrote p.txt")]
    summary = wire.Reply([wire.text("SUMMARY-MARK: the earlier rounds, quoted.")])
    post = {"messages": [opening("SUMMARY-MARK: the earlier rounds, quoted."), deepcopy(history[3]),
                         deepcopy(history[4])]}
    problems, facts = wire.fold_problems(history, post, summary, "unit")
    assert problems == [] and facts == {"before": 5, "after": 3, "folded": 2, "kept": 2}
    lost = deepcopy(post)                          # 留下来的尾巴丢了思考块
    lost["messages"][1]["content"] = without_reasoning(post["messages"][1]["content"])
    assert "untouched assistant-led tail" in wire.fold_problems(history, lost, summary, "unit")[0][0]
    else_head = deepcopy(post)                     # 第一条不是摘要头
    else_head["messages"][0] = opening("something else")
    assert "not the summary head" in wire.fold_problems(history, else_head, summary, "unit")[0][0]


def test_fold_history_rebuilds_what_the_summariser_saw():
    reply = wire.Reply([wire.thinking(wire.THINKING[4], wire.signature("fh")), wire.text("step"),
                        wire.tool_use("write_file", {"path": "a.txt", "content": "x"}, "toolu_fh_1")], "tool_use")
    rebuilt = wire.fold_history(exchange(3, [opening("task")], reply))
    assert rebuilt == [opening("task"), assistant(wire.splice(reply.reasoning, reply.content)),
                       tool_result("toolu_fh_1", "<tool output>")]
    assert wire.fold_history(wire.Exchange(4, {"messages": [opening()]}, None)) == [opening()]   # 没有可用回复


def test_the_summary_request_is_recognised_by_its_shape():
    body = {"messages": [opening("summarise")], "system": "summariser prompt", "stream": True}
    events = wire.reply_events(wire.Reply([wire.text("SUM")]), 3)
    assert wire.Exchange(3, body, events).kind == "summary"
    assert wire.Exchange(3, dict(body, tools=[TOOL]), events).kind == "main"            # 主循环请求带工具表
    assert wire.Exchange(3, dict(body, system=[{"type": "text", "text": "s"}]), events).kind == "main"
    assert wire.Exchange(3, dict(body, messages=[opening(), opening()]), events).kind == "main"
    assert wire.Exchange(3, {"model": "m"}, events).kind == "other"


def test_the_summary_request_must_not_carry_reasoning_or_thinking_fields():
    clean = {"messages": [opening("summarise the earlier rounds")], "system": "summariser prompt"}
    assert wire.summary_request_problems(clean, "unit") == []
    dirty = {"messages": [opening("x"), assistant([wire.thinking("draft", "sig")])],
             "thinking": {"type": "adaptive"}, "output_config": {"effort": "low"}}
    problems = wire.summary_request_problems(dirty, "unit")
    assert len(problems) == 2
    assert "never sees carried reasoning" in problems[0] and "thinking off" in problems[1]


def test_audit_traffic_reports_what_it_cannot_audit():
    """审计说不出口的事要如实说：没有响应只怪响应，请求自己的形状照查（不该再冒出
    「给 run 至少一个工具」那种不相干的指责）。"""
    dead = wire.Exchange(1, {"messages": [opening()], "tools": [TOOL]}, None, "no response was recorded",
                         "request #1", 0)
    assert list(wire.audit_traffic([dead], True, True)["rules"]) == ["response"]
    only_summary = wire.Exchange(1, {"messages": [opening("summarise")]},
                                 wire.reply_events(wire.Reply([wire.text("x")]), 1))
    assert "shape" in wire.audit_traffic([only_summary], True, True)["rules"]    # 没有任何主循环请求
    # 响应半截：请求自己的形状照查，缺的只记在 response 上——不怪工具配置
    cut = wire.Exchange(1, {"messages": [opening()], "tools": [TOOL]}, wire.reply_events(conversation()[0], 1)[:6],
                         "the recorded stream is marked incomplete", "request #1")
    audit = wire.audit_traffic([cut], True, True)
    assert list(audit["rules"]) == ["response"] and "give the run at least one tool" not in audit["rules"]["response"][0]
    assert (audit["stats"]["main"], audit["stats"]["unusable"], audit["stats"]["expected"]) == (1, 1, 0)
    # 形状确实坏了的请求，没有响应也照样报出来
    broken = wire.Exchange(2, {"messages": [assistant([{"type": "thinking", "thinking": "t", "signature": "s"}])]},
                           None, "no response was recorded", "request #2", 0)
    assert list(wire.audit_traffic([broken], True, True)["rules"]) == ["response", "shape"]


# ── usage math ────────────────────────────────────────────────────────────────


def usage_exchange(number, main, **usage):
    body = {"messages": [opening("task" if main else "summarise")]}
    if main:
        body["tools"] = [TOOL]
    reply = wire.Reply([wire.text("u")], "end_turn", usage)
    return wire.Exchange(number, body, wire.reply_events(reply, number))


def test_usage_math_sums_every_request_and_aligns_each_result_file():
    exchanges = [
        usage_exchange(1, True, input_tokens=10, output_tokens=3, cache_creation_input_tokens=1,
                       cache_read_input_tokens=5),
        usage_exchange(2, True, input_tokens=20, output_tokens=4, cache_creation_input_tokens=2,
                       cache_read_input_tokens=6),
        usage_exchange(3, True, input_tokens=30, output_tokens=5, cache_creation_input_tokens=0,
                       cache_read_input_tokens=7),
        usage_exchange(4, False, input_tokens=100, output_tokens=7, cache_creation_input_tokens=0,
                       cache_read_input_tokens=0),
    ]
    assert [item.kind for item in exchanges] == ["main", "main", "main", "summary"]
    first = dict(wire.sum_usage(exchanges[:2]), turns=2)             # 一次 capture 里的两段 run
    assert first == {"input_tokens": 30, "output_tokens": 7, "cache_creation_input_tokens": 3,
                     "cache_read_input_tokens": 11, "turns": 2}
    assert wire.usage_text(exchanges[:2]) == (
        "2 request(s) (2 main + 0 summary): input 30 / output 7 / cache write 3 / cache read 11")
    assert wire.usage_problems(first, exchanges[:2], "unit") == []
    wrong = dict(first, output_tokens=8, reasoning_tokens=0, turns=1)
    problems = wire.usage_problems(wrong, exchanges[:2], "unit")
    assert len(problems) == 3
    for axis, text in zip(("output_tokens", "reasoning_tokens", "turns"), problems):
        assert axis in text
    assert wire.usage_window(first, exchanges, 0) == 2
    second = dict(wire.sum_usage(exchanges[2:]), turns=1)            # 第二段：1 个主循环请求 + 1 个摘要请求
    assert wire.usage_window(second, exchanges, 2) == 4
    assert wire.usage_problems(second, exchanges[2:], "unit") == []
    foreign = dict(second, input_tokens=999)                         # 不是这段流量跑出来的 result.json
    assert wire.usage_window(foreign, exchanges, 2) is None
    off_by_turns = dict(wire.sum_usage(exchanges[2:]), turns=5)      # usage 对得上、turns 对不上
    assert wire.usage_window(off_by_turns, exchanges, 2) == 4
    assert any("turns is 5" in text for text in wire.usage_problems(off_by_turns, exchanges[2:], "unit"))


# ── credential hygiene: what capture records, and what it never does ──────────


def everything(directory):
    """Everything under `directory` as one text — what a leak would have to be in."""
    return "\n".join(path.read_text(encoding="utf-8")
                     for path in sorted(Path(directory).rglob("*")) if path.is_file())


def test_safe_headers_keep_credential_looking_names_by_name_only():
    headers = {
        "x-api-key": FAKE_KEY, "Authorization": FAKE_BEARER, "Cookie": "a=b", "Set-Cookie": "c=d; HttpOnly",
        "X-Auth-Token": "t", "anthropic-version": wire.ANTHROPIC_VERSION,
        "anthropic-beta": "interleaved-thinking-2025-05-14", "content-type": "application/json",
        "x-note": "Authorization: Bearer " + "b" * 24,
    }
    safe = wire.safe_headers(headers)
    for name in ("x-api-key", "authorization", "cookie", "set-cookie", "x-auth-token"):
        assert safe[name] == wire.NAME_ONLY
    assert safe["anthropic-version"] == wire.ANTHROPIC_VERSION
    assert safe["anthropic-beta"] == "interleaved-thinking-2025-05-14"      # 端到端 header 的值照记
    assert safe["content-type"] == "application/json"
    assert safe["x-note"] == "<redacted: key pattern>"                     # 名字不像凭据、值像：打码
    text = json.dumps(safe)
    for secret in (FAKE_KEY, FAKE_BEARER, "HttpOnly", "b" * 24):
        assert secret not in text


def test_find_secrets_names_the_pattern_and_scrub_removes_the_value():
    assert wire.find_secrets("nothing to see, session 42") == []
    assert wire.find_secrets(BODY_KEY) == ["sk-key"]
    assert wire.find_secrets("Authorization: " + FAKE_BEARER) == ["bearer"]
    assert wire.find_secrets('{"api_key": "' + "k" * 20 + '"}') == ["key-field"]
    assert wire.find_secrets('{"password": "short"}') == []             # 太短的不算
    assert wire.find_secrets("task-" + "0" * 22) == ["sk-key"]          # 宁可误报：模式刻意不锚定
    line = json.dumps({"note": BODY_KEY, "auth": FAKE_BEARER, "cfg": {"api_key": "k" * 24}})
    assert wire.find_secrets(line) == ["bearer", "key-field", "sk-key"]
    scrubbed = wire.scrub(line)
    assert wire.find_secrets(scrubbed) == []
    assert json.loads(scrubbed) == {"note": "<redacted:sk-key>", "auth": "<redacted:bearer>",
                                    "cfg": {"api_key": "<redacted:key-field>"}}


def test_the_record_builders_strip_credentials_queries_and_oversize_bodies():
    raw = json.dumps({"model": "m", "note": BODY_KEY}).encode("utf-8")
    record = wire.build_request_record(7, "POST", "/v1/messages?trace=1&key=z",
                                       {"x-api-key": FAKE_KEY, "anthropic-beta": "b-1"}, raw)
    assert record["n"] == 7 and record["method"] == "POST" and record["path"] == "/v1/messages"
    assert record["headers"] == {"x-api-key": wire.NAME_ONLY, "anthropic-beta": "b-1"}
    assert record["body"] == {"model": "m", "note": BODY_KEY}       # 构造器只管形状；打码发生在写入时
    record = wire.build_request_record(8, "POST", "/v1/messages", {}, b"not json " * 8000)
    assert "body" not in record and record["body_truncated"] is True and len(record["body_text"]) == wire.BODY_CAP
    record = wire.build_request_record(9, "GET", "/", {}, b"")
    assert record["body_text"] == "" and record["body_truncated"] is False

    reply = wire.Reply([wire.text("hi")], "end_turn", {"input_tokens": 1, "output_tokens": 2})
    events = wire.reply_events(reply, 3)
    record = wire.build_response_record(3, 200, "OK", {"Content-Type": "text/event-stream",
                                                       "Set-Cookie": "sid=secret"},
                                        events, reply, "", False, None)
    assert record["complete"] is True and record["truncated"] is False and record["error"] is None
    assert record["headers"] == {"content-type": "text/event-stream", "set-cookie": wire.NAME_ONLY}
    assert [frame[0] for frame in record["events"]] == [event for event, _ in events]
    assert record["reply"] == {"blocks": reply.blocks, "stop_reason": "end_turn", "usage": reply.usage}
    assert "body_text" not in record
    cut = wire.build_response_record(4, 200, "OK", {}, [("message_start", "{}")], None, "", True,
                                     "client disconnected")
    assert cut["complete"] is False and cut["truncated"] is True and cut["error"] == "client disconnected"
    assert "reply" not in cut
    plain = wire.build_response_record(5, 400, "Bad Request", {}, [], None, '{"type":"error"}', False, None)
    assert plain["body_text"] == '{"type":"error"}' and "events" not in plain and plain["complete"] is False


def test_forward_headers_drop_hop_by_hop_headers_and_keep_the_rest():
    headers = {"Host": "127.0.0.1:9", "Content-Length": "3", "Accept-Encoding": "gzip",
               "Connection": "keep-alive, X-Drop-Me", "X-Drop-Me": "hop by hop", "X-Keep-Me": "end to end",
               "anthropic-version": wire.ANTHROPIC_VERSION, "anthropic-beta": "b-1", "X-Api-Key": FAKE_KEY}
    forwarded = wire.forward_headers(headers)
    assert [name for name, _ in forwarded] == ["X-Keep-Me", "anthropic-version", "anthropic-beta", "X-Api-Key"]
    assert dict(forwarded)["X-Api-Key"] == FAKE_KEY                  # 凭据要过河：转发原样，只是不落盘


def test_a_refused_address_is_never_echoed():
    place = "PLACEHOLDER.invalid"
    with pytest.raises(ValueError) as refused:
        wire.parse_listen(place + ":8080")                           # 只听 loopback
    assert place not in str(refused.value) and "8080" not in str(refused.value)
    for nonsense in ("127.0.0.1:not-a-port", "localhost"):
        with pytest.raises(ValueError) as error:
            wire.parse_listen(nonsense)
        assert nonsense not in str(error.value)
    assert wire.parse_listen("0") == ("127.0.0.1", 0)
    assert wire.parse_listen("localhost:8080") == ("localhost", 8080)
    for bad in ("ftp://%s:9/x" % place, "http://%s:9/x?q=1" % place, "http://%s:not-a-port" % place, "http://"):
        with pytest.raises(ValueError) as error:
            wire.Upstream(bad, 5.0)
        assert place not in str(error.value) and "not-a-port" not in str(error.value) and "q=1" not in str(error.value)
    upstream = wire.Upstream("http://user:password@127.0.0.1:9/prefix/", 5.0)
    assert (upstream.host, upstream.port, upstream.prefix, upstream.secure) == ("127.0.0.1", 9, "/prefix", False)
    assert upstream.target("/v1/messages") == "/prefix/v1/messages"
    assert "password" not in json.dumps(vars(upstream))              # userinfo 用完即弃，不留在对象里
    secure = wire.Upstream("https://%s" % place, 5.0)
    assert (secure.port, secure.prefix, secure.secure) == (443, "", True)


def test_capture_state_scrubs_counts_and_rescans(tmp_path):
    out = tmp_path / "cap"
    state = wire.CaptureState(str(out))
    kinds = state.write("requests.jsonl", {"n": 1, "body": {"api_key": BODY_KEY, "note": FAKE_BEARER, "ok": "x"}})
    assert kinds == ["bearer", "key-field", "sk-key"]
    state.write("responses.jsonl", {"n": 1, "status": 200, "complete": True})
    assert state.count == {"requests": 1, "responses": 1, "complete": 1, "redacted": 1}
    written = everything(out)
    assert BODY_KEY not in written and FAKE_BEARER not in written and "<redacted:" in written
    assert state.close() == {}                                       # 复扫：文件里已经没有密钥样串
    meta = json.loads((out / "meta.json").read_text(encoding="utf-8"))
    assert meta["upstream"] == "<not recorded>" and meta["ended_at"]
    assert meta["leak_scan"]["redacted_records"] == [
        {"file": "requests.jsonl", "line": 1, "patterns": ["bearer", "key-field", "sk-key"]}]
    assert wire.capture_verdict(1, {}) != []                         # 没有明文落盘，但这次 capture 仍然红
    closed = everything(out)
    assert state.write("requests.jsonl", {"n": 2}) == [] and everything(out) == closed   # 关闭之后不再写
    with pytest.raises(OSError):
        wire.CaptureState(str(out))                                  # 既有 capture 永不覆盖
    # 绕过 write() 落盘的密钥样串：复扫抓得住，裁决红
    (out / "meta.json").write_text((out / "meta.json").read_text(encoding="utf-8") + BODY_KEY + "\n",
                                   encoding="utf-8")
    assert wire.scan_capture(str(out)) == {"meta.json": ["sk-key"]}
    verdict = wire.capture_verdict(0, {"meta.json": ["sk-key"]})
    assert len(verdict) == 1 and "meta.json" in verdict[0] and "sk-key" in verdict[0]
    assert wire.capture_verdict(0, {}) == []


def test_capture_summary_names_the_credential_headers_not_their_values():
    text = wire.capture_summary({"requests": 2, "responses": 2, "complete": 1}, {"x-api-key", "authorization"},
                                "/tmp/x")
    assert text.startswith("[capture] 2 request(s) recorded, 2 answered (1 complete stream(s))")
    assert "seen: authorization, x-api-key" in text and "audit /tmp/x" in text
    empty = wire.capture_summary({"requests": 0, "responses": 0, "complete": 0}, set(), "/tmp/x")
    assert empty.count("seen: none") == 1


# ── the capture proxy, against this file's own mock endpoint ──────────────────
#
# The upstream of every proxy below is a MockServer started a few lines above it:
# nothing in this section talks to anything but 127.0.0.1.

GATE_SECS = 8.0          # how long the gated endpoint holds a stream before giving up


def free_port():
    """A loopback port nobody listens on right now (for an unreachable upstream)."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


@contextlib.contextmanager
def serving(server):
    """`server` (a MockServer) answering for the duration of the block."""
    thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.2}, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()


@contextlib.contextmanager
def proxying(out_dir, upstream_url, **options):
    """A started capture proxy in front of `upstream_url`; stopped — files
    closed and re-scanned — when the block ends."""
    proxy = wire.CaptureProxy(str(out_dir), upstream_url, **options).start()
    try:
        yield proxy
    finally:
        proxy.stop()


class GatedHandler(wire.MockHandler):
    """The mock endpoint holding its stream open: the frames up to the first
    content_block_stop go out at once, the rest only once the test releases
    `server.gate` — in small pieces, so a client that has left is noticed."""

    def _stream(self, events):
        cut = [index for index, (event, _) in enumerate(events) if event == "content_block_stop"][0] + 1
        head = wire.sse_text(events[:cut]).encode("utf-8")
        rest = wire.sse_text(events[cut:]).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        self.wfile.write(b"%x\r\n%s\r\n" % (len(head), head))
        self.server.gate.wait(GATE_SECS)
        for start in range(0, len(rest), 200):
            piece = rest[start:start + 200]
            self.wfile.write(b"%x\r\n%s\r\n" % (len(piece), piece))
            self.wfile.flush()
            time.sleep(0.05)
        self.wfile.write(b"0\r\n\r\n")


class GatedServer(wire.MockServer):
    def __init__(self, address, mock):
        self.mock, self.gate = mock, threading.Event()
        wire.LoopbackServer.__init__(self, address, GatedHandler)


def api_request(messages):
    """A Messages API request body the way a client puts it on the wire."""
    return {"model": "mock-claude", "max_tokens": 512, "stream": True, "messages": messages, "tools": [TOOL]}


def wire_text(frames):
    """[(event, raw data)] of a response record back to the bytes they arrived as."""
    return "".join("event: %s\ndata: %s\n\n" % (event, data) for event, data in frames).encode("utf-8")


def raw_request(target, body, headers, chunked=False):
    """The bytes of a POST to `target` (origin form or absolute form)."""
    lines = ["POST %s HTTP/1.1" % target, "Host: 127.0.0.1"]
    lines += ["%s: %s" % (name, value) for name, value in headers.items()]
    payload = json.dumps(body).encode("utf-8")
    if chunked:
        lines.append("Transfer-Encoding: chunked")
        sent = b"".join(b"%x\r\n%s\r\n" % (len(piece), piece)
                        for piece in (payload[start:start + 7] for start in range(0, len(payload), 7)))
        sent += b"0\r\n\r\n"
    else:
        lines.append("Content-Length: %d" % len(payload))
        sent = payload
    return ("\r\n".join(lines) + "\r\n\r\n").encode("utf-8") + sent


def relay(port, request):
    """Send `request` bytes untouched to the proxy on `port`. Returns
    (socket, HTTPResponse) with the response head read and no body: the caller
    decides how much to read, and when to walk away."""
    sock = socket.create_connection(("127.0.0.1", port), timeout=20)
    response = http.client.HTTPResponse(sock, method="POST")
    sock.sendall(request)
    response.begin()
    return sock, response


def read_until(response, marker):
    """The start of a stream up to and including the line `marker`."""
    seen = []
    while True:
        line = response.readline()
        assert line, "the stream ended before %r" % marker
        seen.append(line)
        if line == marker:
            return b"".join(seen)


def await_record(directory, number, seconds=5.0):
    """The response record of request `number`, once the proxy has written it."""
    deadline = time.time() + seconds
    while time.time() < deadline:
        for record in wire.read_jsonl(os.path.join(directory, "responses.jsonl")) or []:
            if record and record.get("n") == number:
                return record
        time.sleep(0.02)
    raise AssertionError("the proxy wrote no response record for request #%d within %.0f s" % (number, seconds))


def captured(tmp_path, name="cap", extra=None):
    """A capture directory recorded through the proxy: two requests of one
    faithful conversation (request #2 echoes the thinking block of reply #1).
    Returns the directory."""
    reply, final, echoed, _ = conversation()
    mock = wire.Mock()
    mock.begin("cap", [reply, final])
    directory = tmp_path / name
    with serving(wire.MockServer(("127.0.0.1", 0), mock)) as endpoint:
        with proxying(directory, endpoint.base_url) as proxy:
            for messages in ([opening()], echoed):
                body = api_request(messages)
                body.update(extra or {})
                sock, response = relay(proxy.port, raw_request("/v1/messages", body, CLIENT_HEADERS))
                assert response.status == 200
                response.read()
                response.close()
                sock.close()
    return str(directory)


def edit_capture(directory, name, edit):
    """Rewrite one capture file with `edit(records)` applied in place — the
    capture of a kernel that sends something else."""
    path = os.path.join(directory, name)
    records = wire.read_jsonl(path)
    edit(records)
    with open(path, "w", encoding="utf-8") as handle:
        handle.writelines(json.dumps(record, ensure_ascii=False) + "\n" for record in records)


def audit(*argv):
    """The audit subcommand in-process: (exit code, what it printed)."""
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        code = wire.cmd_audit(wire.build_parser().parse_args(["audit"] + list(argv)))
    return code, out.getvalue()


def write_json(path, value):
    with open(path, "w", encoding="utf-8") as handle:
        json.dump(value, handle, ensure_ascii=False, indent=2)
    return str(path)


def test_the_servers_bind_without_a_reverse_lookup(tmp_path, monkeypatch):
    """标准库 HTTPServer.server_bind() 会对绑定地址做一次阻塞的反向 DNS 查询（socket.getfqdn）——
    反向解析卡住的机器上，两个子命令都会在打印任何一行之前挂死（本机某个 3.13 构建上实发）。
    mock 端点与 capture 代理只绑回环，用不上那个名字：这里的 server_bind 不得查。"""
    def forbidden(name=""):
        raise AssertionError("socket.getfqdn(%r) — a blocking reverse lookup" % name)

    monkeypatch.setattr(socket, "getfqdn", forbidden)
    with serving(wire.MockServer(("127.0.0.1", 0), wire.Mock())) as endpoint:
        assert endpoint.base_url.startswith("http://127.0.0.1:") and endpoint.server_name == "127.0.0.1"
        with proxying(tmp_path / "cap", endpoint.base_url) as proxy:
            assert proxy.base_url.startswith("http://127.0.0.1:") and proxy.server.server_port == proxy.port


def test_the_proxy_relays_the_stream_and_records_both_directions(tmp_path, capsys):
    """流原样转发（逐字节）；请求记端到端 header（凭据类只留名字）与 JSON 体、路径不带查询串；响应记原始帧。"""
    reply, final, echoed, _ = conversation()
    mock = wire.Mock()
    mock.begin("relay", [reply, final])
    out = tmp_path / "cap"
    port = free_port()
    with serving(wire.MockServer(("127.0.0.1", 0), mock)) as endpoint:
        with proxying(out, endpoint.base_url, listen="127.0.0.1:%d" % port) as proxy:
            assert proxy.port == port and proxy.base_url == "http://127.0.0.1:%d" % port
            streams = []
            for messages in ([opening()], echoed):
                sock, response = relay(port, raw_request("/v1/messages?trace=1", api_request(messages), CLIENT_HEADERS))
                assert response.status == 200 and response.getheader("Content-Type") == "text/event-stream"
                streams.append(response.read())
                response.close()
                sock.close()
    assert streams == [wire.sse_text(wire.reply_events(sent, number)).encode("utf-8")
                       for sent, number in zip((reply, final), (1, 2))]

    requests = wire.read_jsonl(str(out / "requests.jsonl"))
    responses = wire.read_jsonl(str(out / "responses.jsonl"))
    assert [record["n"] for record in requests] == [1, 2]
    assert [record["n"] for record in responses] == [1, 2]
    assert requests[0]["method"] == "POST" and requests[0]["path"] == "/v1/messages"
    assert requests[0]["headers"] == {
        "x-api-key": wire.NAME_ONLY, "authorization": wire.NAME_ONLY,
        "anthropic-version": wire.ANTHROPIC_VERSION, "anthropic-beta": "interleaved-thinking-2025-05-14",
        "content-type": "application/json", "accept": "text/event-stream", "x-keep-me": "end to end"}
    assert requests[1]["body"] == api_request(echoed)
    for record, sent in zip(responses, (reply, final)):
        payload = wire.sse_text(wire.reply_events(sent, record["n"])).encode("utf-8")
        assert wire_text(record["events"]) == payload               # 帧按到达时的原样记录
        assert record["complete"] is True and record["truncated"] is False and record["error"] is None
        assert record["reply"] == {"blocks": sent.blocks, "stop_reason": sent.stop_reason, "usage": sent.usage}
    seen = mock.records[0]["headers"]                         # 上游那边看到的
    assert "x-api-key" in seen                                # 凭据要过河（mock 同样只记名字）
    assert seen["anthropic-beta"] == "interleaved-thinking-2025-05-14"
    assert seen["anthropic-version"] == wire.ANTHROPIC_VERSION and seen["x-keep-me"] == "end to end"
    assert "x-drop-me" not in seen and seen["accept-encoding"] == "identity"

    meta = json.loads((out / "meta.json").read_text(encoding="utf-8"))
    assert meta["credential_header_names_seen"] == ["authorization", "x-api-key"]
    assert meta["counts"] == {"requests": 2, "responses": 2, "complete": 2, "redacted": 0}
    assert meta["leak_scan"] == {"patterns": ["sk-key", "bearer", "key-field"], "redacted_records": [],
                                 "hits_in_files": {}}
    assert meta["upstream"] == "<not recorded>" and meta["ended_at"]
    written = everything(out)
    for leak in (FAKE_KEY, "raw-bearer", "127.0.0.1", "trace=1", "dummy-key"):
        assert leak not in written, leak
    assert "proxy thread failed" not in capsys.readouterr().out


def test_the_proxy_streams_it_does_not_buffer_the_response(tmp_path):
    """端点扣着后半段不放，客户端已经收到前半段：代理是逐段转发的，不是攒完再给。"""
    reply, _, _, _ = conversation()
    mock = wire.Mock()
    mock.begin("live", [reply])
    with serving(GatedServer(("127.0.0.1", 0), mock)) as endpoint:
        with proxying(tmp_path / "cap", endpoint.base_url) as proxy:
            started = time.time()
            sock, response = relay(proxy.port, raw_request("/v1/messages", api_request([opening()]), CLIENT_HEADERS))
            head = read_until(response, b"event: content_block_stop\n")
            assert time.time() - started < GATE_SECS / 2        # 端点没放手，头已经到了
            endpoint.gate.set()
            stream = head + response.read()
            response.close()
            sock.close()
    assert stream == wire.sse_text(wire.reply_events(reply, 1)).encode("utf-8")
    record = wire.read_jsonl(str(tmp_path / "cap" / "responses.jsonl"))[0]
    assert record["complete"] is True and record["error"] is None


def test_a_client_that_leaves_mid_stream_is_recorded_as_such(tmp_path, capsys):
    """客户端中途走人：记成 truncated + client disconnected、不算完整；代理接着服务下一位，线程不炸。"""
    reply, final, _, _ = conversation()
    mock = wire.Mock()
    mock.begin("gone", [reply, final])
    out = tmp_path / "cap"
    with serving(GatedServer(("127.0.0.1", 0), mock)) as endpoint:
        with proxying(out, endpoint.base_url) as proxy:
            sock, response = relay(proxy.port, raw_request("/v1/messages", api_request([opening()]), CLIENT_HEADERS))
            read_until(response, b"event: content_block_stop\n")
            response.close()                                   # 端点还扣着后半段，客户端已经走了
            sock.close()
            endpoint.gate.set()
            cut = await_record(str(out), 1)
            assert cut["truncated"] is True and cut["complete"] is False
            assert cut["error"] == "client disconnected"     # 记下的帧是端点发过的；客户端没收全
            sock, response = relay(proxy.port, raw_request("/v1/messages", api_request([opening()]), CLIENT_HEADERS))
            assert response.status == 200
            assert response.read() == wire.sse_text(wire.reply_events(final, 2)).encode("utf-8")
            response.close()
            sock.close()
    assert "proxy thread failed" not in capsys.readouterr().out
    code, out = audit(str(out))
    assert code == 1 and "[FAIL] audit: responses — request #1: the response is incomplete" in out
    assert "[PASS] audit: request shape — 2 request(s)" in out            # 请求自己的形状照查，只怪响应
    assert "1 request(s) with no usable response, not checked for echo" in out
    assert "give the run at least one tool" not in out


def test_stopping_the_proxy_drops_a_record_that_arrives_too_late(tmp_path, monkeypatch, capsys):
    """停机时还在途的请求只等一小会儿；之后才到的记录被丢弃，而不是写进已关闭的文件。"""
    monkeypatch.setattr(wire, "DRAIN_SECS", 0.3)
    reply, _, _, _ = conversation()
    mock = wire.Mock()
    mock.begin("late", [reply])
    out = tmp_path / "cap"
    with serving(GatedServer(("127.0.0.1", 0), mock)) as endpoint:
        started = time.time()
        with proxying(out, endpoint.base_url) as proxy:
            sock, response = relay(proxy.port, raw_request("/v1/messages", api_request([opening()]), CLIENT_HEADERS))
            read_until(response, b"event: content_block_stop\n")
        assert time.time() - started >= wire.DRAIN_SECS          # stop() 等了在途的请求，然后才收摊
        endpoint.gate.set()                                     # 端点现在才放完后半段
        assert response.read()                                  # 客户端照样收完
        response.close()
        sock.close()
    assert "proxy thread failed" not in capsys.readouterr().out
    assert len(wire.read_jsonl(str(out / "requests.jsonl"))) == 1
    assert wire.read_jsonl(str(out / "responses.jsonl")) == []
    meta = json.loads((out / "meta.json").read_text(encoding="utf-8"))
    assert meta["counts"] == {"requests": 1, "responses": 0, "complete": 0, "redacted": 0}


def test_an_unreachable_upstream_is_a_502_without_its_address(tmp_path, capsys):
    """上游连不上：502 JSON 只带异常类名，不带上游地址，代理线程不炸。"""
    out = tmp_path / "cap"
    with proxying(out, "http://127.0.0.1:%d" % free_port()) as proxy:
        sock, response = relay(proxy.port, raw_request("/v1/messages", api_request([opening()]), CLIENT_HEADERS))
        assert response.status == 502
        payload = json.loads(response.read())
        response.close()
        sock.close()
    assert payload == {"type": "error", "error": {"type": "upstream_unreachable", "message": "ConnectionRefusedError"}}
    record = wire.read_jsonl(str(out / "responses.jsonl"))[0]
    assert record["status"] == 502 and record["complete"] is False and record["error"] == "ConnectionRefusedError"
    assert "127.0.0.1" not in everything(out)
    assert "proxy thread failed" not in capsys.readouterr().out
    assert audit(str(out))[0] == 1


def test_chunked_bodies_and_absolute_form_targets_are_understood(tmp_path):
    """请求体是 chunked、请求行是 absolute-form：照样转发，落盘的路径不带查询串。"""
    mock = wire.Mock()
    mock.begin("chunked", [wire.Reply([wire.text("ok")], "end_turn", wire.usage_for(1))])
    with serving(wire.MockServer(("127.0.0.1", 0), mock)) as endpoint:
        with proxying(tmp_path / "cap", endpoint.base_url) as proxy:
            body = api_request([opening("chunked")])
            request = raw_request("http://127.0.0.1/v1/messages?x=1", body, CLIENT_HEADERS, chunked=True)
            sock, response = relay(proxy.port, request)
            assert response.status == 200                       # 路径走歪了 mock 会答 404
            assert wire.reassemble(wire.parse_sse(response.read().decode("utf-8")))["blocks"] == [
                {"type": "text", "text": "ok"}]
            response.close()
            sock.close()
    record = wire.read_jsonl(str(tmp_path / "cap" / "requests.jsonl"))[0]
    assert record["path"] == "/v1/messages" and record["body"] == body
    assert mock.records[0]["error"] is None


def test_a_key_inside_a_request_body_is_scrubbed_before_it_is_written(tmp_path):
    """体里夹带密钥样串（工具读了配置文件的那种）：写盘前打码并记账，capture 裁决红，别的核对照常。"""
    directory = captured(tmp_path, extra={"metadata": {"access_token": BODY_KEY}})
    written = everything(directory)
    assert BODY_KEY not in written and "<redacted:" in written
    meta = json.loads((Path(directory) / "meta.json").read_text(encoding="utf-8"))
    assert meta["counts"]["redacted"] == 2                      # 两个请求都带着它
    assert meta["leak_scan"]["redacted_records"] == [
        {"file": "requests.jsonl", "line": 1, "patterns": ["key-field", "sk-key"]},
        {"file": "requests.jsonl", "line": 2, "patterns": ["key-field", "sk-key"]}]
    assert meta["leak_scan"]["hits_in_files"] == {}
    assert len(wire.capture_verdict(2, {})) == 1                 # 没有明文落盘，这次 capture 仍然不算干净
    assert audit(directory)[0] == 0


def test_audit_passes_on_a_faithful_capture(tmp_path):
    code, out = audit(captured(tmp_path))
    assert code == 0 and "[FAIL]" not in out and "[GAP]" not in out
    assert "[PASS] audit: echo fidelity — 1 reasoning block(s) re-sent byte-identical (text, signature, " \
           "position) across 1 request(s)" in out
    assert "[note] carry read as on: the wire echoes reasoning blocks" in out
    assert "[PASS] self-test: mutations" in out
    assert out.rstrip().splitlines()[-1].startswith("[PASS] audit_anthropic_wire: ")


CAPTURE_TAMPERING = {
    # label -> (how the re-sent assistant content is edited, the check that must go red, its wording)
    "signature altered": (WIRE_MUTATIONS["signature altered"][0], "audit: echo fidelity", "signature differs"),
    "text altered": (WIRE_MUTATIONS["text altered"][0], "audit: echo fidelity", "text differs at char"),
    "position moved": (thinking_to_the_end, "audit: echo fidelity", "re-sent before content block"),
    "block dropped": (without_reasoning, "audit: echo fidelity", "expected 1 reasoning block(s)"),
    "tool input changed": (WIRE_MUTATIONS["tool input changed"][0], "audit: reply content", "input changed"),
}


@pytest.mark.parametrize("label", sorted(CAPTURE_TAMPERING))
def test_audit_goes_red_on_a_tampered_capture(tmp_path, label):
    """线上的回传被改了签名 / 正文 / 位置，整块丢了，或者工具入参变了：同一个 audit 必须红。"""
    mutate, check, wording = CAPTURE_TAMPERING[label]

    def re_sent_differently(records):
        message = records[1]["body"]["messages"][1]
        message["content"] = mutate(message["content"])

    directory = captured(tmp_path)
    edit_capture(directory, "requests.jsonl", re_sent_differently)
    code, out = audit(directory)
    assert code == 1 and ("[FAIL] %s — " % check) in out
    assert wording in out and ("[PASS] %s — " % check) not in out
    assert out.rstrip().splitlines()[-1].startswith("[FAIL] audit_anthropic_wire: ")


def test_audit_passes_on_a_harmless_reencoding(tmp_path):
    """只是键序变了：不算篡改。"""
    directory = captured(tmp_path)
    edit_capture(directory, "requests.jsonl", lambda records: records[1]["body"]["messages"][1].update(
        content=reordered(records[1]["body"]["messages"][1]["content"])))
    assert audit(directory)[0] == 0


def test_audit_holds_a_carry_off_run_to_no_echo(tmp_path):
    """同一条 capture：按开核对是绿的，声明 --carry-off 之后那一个回传就成了违规。"""
    directory = captured(tmp_path)
    code, out = audit(directory, "--carry-off")
    assert code == 1 and "[FAIL] audit: echo fidelity — " in out and "expected 0 reasoning block(s)" in out
    code, out = audit(directory, "--carry-off", "--only", "echo fidelity")
    assert code == 1 and "carry read as off (--carry-off)" in out
    assert [line for line in out.splitlines() if line.startswith("[PASS]")][0].startswith(
        "[PASS] self-test: mutations")


def test_audit_holds_the_session_file_to_both_sides(tmp_path):
    directory = captured(tmp_path)
    reply, final, _, _ = conversation()
    session = write_json(tmp_path / "session.json", session_file(reply, final))
    code, out = audit(directory, "--session", session)
    assert code == 0 and "[PASS] audit: session three-way (session session.json)" in out
    broken = session_file(reply, final)
    STORED_MUTATIONS["signature altered"][0](broken["messages"][1])
    code, out = audit(directory, "--session", write_json(tmp_path / "bad.json", broken))
    assert code == 1 and "[FAIL] audit: session three-way (session bad.json)" in out
    code, out = audit(directory, "--session", str(tmp_path / "nope.json"))
    assert code == 1 and "[FAIL] audit: session file nope.json — no readable JSON" in out
    code, out = audit(directory, "--session", write_json(tmp_path / "other.json", {"hello": 1}))
    assert code == 1 and "[FAIL] audit: session file other.json — not a session file" in out


def test_audit_aligns_each_result_file_to_its_own_requests(tmp_path):
    directory = captured(tmp_path)
    exchanges = wire.capture_exchanges(directory)[0]
    result = write_json(tmp_path / "result.json", dict(wire.sum_usage(exchanges), turns=2))
    code, out = audit(directory, "--result", result)
    assert code == 0 and "[PASS] audit: result.json #1 (result.json, requests #1–#2)" in out
    code, out = audit(directory, "--result", write_json(tmp_path / "elsewhere.json", dict(wire.sum_usage(exchanges),
                                                                                         input_tokens=999)))
    assert code == 1 and "[FAIL] audit: result.json #1 — " in out and "was this run made through the proxy?" in out
    code, out = audit(directory, "--result", write_json(tmp_path / "note.json", {"note": "not a result"}))
    assert code == 1 and "[FAIL] audit: result.json #1 — note.json is not a result.json" in out


def test_audit_names_a_broken_capture(tmp_path):
    directory = captured(tmp_path)
    os.remove(os.path.join(directory, "responses.jsonl"))
    code, out = audit(directory)
    assert code == 1 and "[FAIL] audit: responses" in out
    assert audit(str(tmp_path / "nowhere"))[0] == 1             # 根本没有 capture
    empty = tmp_path / "empty"
    empty.mkdir()
    (empty / "requests.jsonl").write_text("not json\n", encoding="utf-8")
    code, out = audit(str(empty))
    assert code == 1 and "[FAIL] audit: capture files — requests.jsonl line 1 is not a request record" in out
    assert "[FAIL] audit: traffic" in out


def test_audit_fails_on_a_key_left_in_the_capture_directory(tmp_path):
    directory = captured(tmp_path)
    with open(os.path.join(directory, "meta.json"), "a", encoding="utf-8") as handle:
        handle.write(BODY_KEY + "\n")                           # 绕过 write() 落盘的
    code, out = audit(directory)
    assert code == 1 and "[FAIL] audit: leak scan — meta.json matches the sk-key pattern" in out
    assert BODY_KEY not in out                                  # 报的是模式名，不是命中的内容


def test_capture_refuses_to_start_without_echoing_what_it_refused(tmp_path):
    """起不来的三种情形都是一行 [FAIL] + 退出码 1：不回显被拒的地址，不碰既有 capture。"""
    place = "PLACEHOLDER.invalid"
    fresh = str(tmp_path / "fresh")

    def run(*argv):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = wire.main(["capture"] + list(argv))
        return code, out.getvalue()

    code, out = run("--upstream", "http://127.0.0.1:9", "--listen", place + ":8080", "--out", fresh)
    assert code == 1 and out.startswith("[FAIL] capture: setup — the capture proxy listens on loopback only")
    assert place not in out and "8080" not in out and not os.path.exists(fresh)
    code, out = run("--upstream", "http://user:password@%s:9/x?q=1" % place, "--out", fresh)
    assert code == 1 and out.startswith("[FAIL] capture: setup — ")
    for private in (place, "password", "user", "q=1"):
        assert private not in out
    assert not os.path.exists(fresh)
    directory = captured(tmp_path)
    before = everything(directory)
    code, out = run("--upstream", "http://127.0.0.1:9", "--out", directory)
    assert code == 1 and "already holds a capture" in out and everything(directory) == before


def first_line(proc, seconds=20.0):
    """The first stdout line of `proc`; a start that prints nothing fails the test instead of hanging it."""
    deadline = time.time() + seconds
    with selectors.DefaultSelector() as picker:
        picker.register(proc.stdout, selectors.EVENT_READ)
        while picker.select(max(0.0, deadline - time.time())):
            return proc.stdout.readline()
    raise AssertionError("the capture process printed nothing for %.0f s" % seconds)


def capture_subprocess(endpoint_url, directory, bodies):
    """`capture` as a real process in front of `endpoint_url`: wait for it to
    announce its address, send it `bodies`, stop it with SIGINT. Returns
    (exit code, stdout, stderr)."""
    proc = subprocess.Popen(
        [sys.executable, str(SCRIPT), "capture", "--upstream", endpoint_url, "--out", directory],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1"))
    try:
        first = first_line(proc)
        assert first.startswith("[capture] base_url for the scratch config: http://127.0.0.1:"), first
        port = int(first.rstrip().rsplit(":", 1)[1])
        for body in bodies:
            sock, response = relay(port, raw_request("/v1/messages", body, CLIENT_HEADERS))
            assert response.status == 200
            response.read()
            response.close()
            sock.close()
        await_record(directory, len(bodies))
        if hasattr(signal, "SIGINT"):
            proc.send_signal(signal.SIGINT)
        else:
            proc.terminate()
        out, err = proc.communicate(timeout=20)
    finally:
        if proc.poll() is None:                     # a proxy that forgot to stop must not outlive the test
            proc.kill()
            proc.communicate()
    return proc.returncode, out, err


@pytest.mark.skipif(not hasattr(signal, "SIGINT"), reason="a graceful stop needs SIGINT")
@pytest.mark.parametrize("leaky", [False, True])
def test_the_capture_subcommand_stops_cleanly_and_judges_its_own_files(tmp_path, leaky):
    """真实子命令进程：Ctrl-C 收摊、退出码看泄漏裁决；凭据值从不落盘，上游地址与端口也不落盘。"""
    mock = wire.Mock()
    mock.begin("cli", [wire.Reply([wire.text("ok")], "end_turn", wire.usage_for(1))])
    directory = str(tmp_path / "cli")
    body = api_request([opening()])
    if leaky:
        body["metadata"] = {"access_token": BODY_KEY}
    with serving(wire.MockServer(("127.0.0.1", 0), mock)) as endpoint:
        code, out, err = capture_subprocess(endpoint.base_url, directory, [body])
    if leaky:
        assert code == 1 and "[FAIL] capture: leak scan — 1 record(s) matched a key pattern and were redacted" in out
    else:
        assert code == 0, err
        assert "[PASS] capture: leak scan — no key pattern in requests.jsonl, responses.jsonl, meta.json" in out
    assert "[capture] 1 request(s) recorded, 1 answered (1 complete stream(s)); credential-looking header " \
           "name(s) seen: authorization, x-api-key" in out
    assert "[capture] audit it offline: python3 scripts/e2e_anthropic_wire.py audit %s " % directory in out
    written = everything(directory)
    for private in (FAKE_KEY, "raw-bearer", BODY_KEY, "127.0.0.1"):
        assert private not in written, private


# ── the real binary: check end to end, and once through the capture proxy ─────
#
# Everything in this section is skipped when target/debug/tiny-agent (or TA_BIN)
# is missing — see NEEDS_BINARY.


def recording_scratch(monkeypatch):
    """让 check() 建的临时目录对测试可见：返回它们落进的列表
    （check() 不被要求 --keep 时会自己清掉）。"""
    created = []
    real_mkdtemp = tempfile.mkdtemp

    def mkdtemp(*args, **kwargs):
        path = real_mkdtemp(*args, **kwargs)
        created.append(path)
        return path

    monkeypatch.setattr(tempfile, "mkdtemp", mkdtemp)      # 脚本 import 的正是这同一个模块对象
    return created


def summary_of(out):
    """最后一行（总结行）解析出的 (状态, passed, failed, known gap(s), 失败项的名字)。
    两种写法：`N passed, 0 failed, M known gap(s)` 与 `K failed (a, b), N passed, M known gap(s)`。"""
    last = out.splitlines()[-1]
    found = re.match(r"^\[(PASS|FAIL)\] e2e_anthropic_wire: (?:(\d+) failed \(([^)]*)\), )?(\d+) passed, (?:0 failed, )?"
                     r"(\d+) known gap\(s\)(?: \(subset: --only [^)]*\))? — [\d.]+ s$", last)
    assert found, last
    return (found.group(1), int(found.group(4)), int(found.group(2) or 0), int(found.group(5)), found.group(3) or "")


SYNTHETIC_GAP = "a gap filed by the test, not by the kernel"


def file_synthetic_gap(monkeypatch, count=3):
    """登记一条合成的 lifetime 缺口，并让审计在场景里凭空报出 count 条它的问题。

    内核此刻没有已登记的缺口（KNOWN_GAPS 是空的），而缺口机制的三条行为——[GAP] 不计失败、
    --verbose 逐条列出、--strict 致命——只有真有一条缺口时才测得到；靠「真缺口还在」的测试在它
    被修掉的那一刻就空转了（2026-11 修掉 --resume --task 那条之后，两条测试就是这么绿的）。
    所以缺口由测试自己合成，机制照常走完整条 main()。multi-turn 场景里带 lifetime 规则的检查
    有两条（request shape 与 new user turn ends the carry），各报全部 count 条。"""
    monkeypatch.setattr(wire, "STRICT", False)
    monkeypatch.setattr(wire, "KNOWN_GAPS", {"lifetime": SYNTHETIC_GAP})
    real = wire.audit_traffic

    def audit_traffic(exchanges, carry_on, strict_sources=False, detail=None):
        audit = real(exchanges, carry_on, strict_sources, detail)
        audit["rules"]["lifetime"] = ["synthetic stale carry %d" % number for number in range(1, count + 1)]
        return audit

    monkeypatch.setattr(wire, "audit_traffic", audit_traffic)


@NEEDS_BINARY
def test_the_real_binary_through_the_proxy_audits_clean(tmp_path):
    """真实二进制 → capture 代理 → mock 端点，再带着会话文件与 result.json 两个见证 audit：
    全绿；记录里的 x-api-key 只剩名字，而端点拿到的是真值（拿不到它就 400，run 不会 ok）。"""
    scratch = tmp_path / "scratch"
    os.makedirs(scratch)
    mock, directory = wire.Mock(), tmp_path / "cap"
    with serving(wire.MockServer(("127.0.0.1", 0), mock)) as endpoint:
        with proxying(directory, endpoint.base_url) as proxy:
            sc = wire.Scenario("cap", mock, proxy.base_url, str(scratch),
                               wire.run_env(str(scratch), {wire.KEY_ENV: "dummy"}))
            item = wire.run(sc, "run", wire.two_reply_script("cap", "Captured."), task="Write cap.txt.")
    assert item.ok, "%s — %s" % (item.describe(), item.stderr)
    requests = wire.read_jsonl(str(directory / "requests.jsonl"))
    assert [record["n"] for record in requests] == [1, 2]
    assert all(record["headers"]["x-api-key"] == wire.NAME_ONLY for record in requests)
    meta = wire.load_json(str(directory / "meta.json"))
    assert meta["credential_header_names_seen"] == ["x-api-key"]
    assert meta["leak_scan"] == {"patterns": [name for name, _ in wire.SECRET_PATTERNS],
                                 "redacted_records": [], "hits_in_files": {}}
    code, out = audit(str(directory), "--session", sc.session, "--result", os.path.join(sc.dir, "result-1.json"))
    assert code == 0 and "[FAIL]" not in out, out
    for line in ("[PASS] audit: echo fidelity — 1 reasoning block(s) re-sent byte-identical",
                 "[PASS] audit: session three-way (session session.json) — ",
                 "[PASS] audit: result.json #1 (result-1.json, requests #1–#2) — "):
        assert line in out, line
    assert out.splitlines()[-1].startswith("[PASS] audit_anthropic_wire: ")


@NEEDS_BINARY
def test_check_runs_every_scenario_and_cleans_up_after_itself(capsys, monkeypatch):
    """整场 check（CI 跑的就是它）：自测在、每个场景都跑到它的最后一条检查、没有 [FAIL]、
    已登记的缺口只以 [GAP] 出现并写明原委、总结行的计数与逐行对得上、临时目录清干净。"""
    monkeypatch.setattr(wire, "STRICT", False)             # 开发者环境里的 TA_WIRE_STRICT 不许说话
    created = recording_scratch(monkeypatch)
    assert wire.main(["check"]) == 0
    lines = capsys.readouterr().out.splitlines()
    assert "[PASS] self-test: mutations — %s" % wire.SELF_TEST_OK in lines
    for name, _ in wire.SCENARIOS:
        for check in ("run outcome", "echo fidelity", "session three-way"):
            assert any(line.startswith("[PASS] %s: %s" % (name, check)) for line in lines), (name, check)
        # 每个场景都给出形状结论：通过，或者（已登记的缺口）如实报成 [GAP]。
        assert any(line.startswith(("[PASS] %s: request shape" % name, "[GAP] %s: request shape" % name))
                   for line in lines), name
    assert not any(line.startswith("[FAIL]") for line in lines)
    for line in (line for line in lines if line.startswith("[GAP] ")):
        assert "KNOWN KERNEL GAP" in line and "--strict makes it fatal" in line, line
        assert any(reason in line for reason in wire.KNOWN_GAPS.values()), line
    passed = sum(1 for line in lines[:-1] if line.startswith("[PASS] "))
    gaps = sum(1 for line in lines if line.startswith("[GAP] "))
    assert summary_of("\n".join(lines)) == ("PASS", passed, 0, gaps, "")
    assert len(created) == 1 and not os.path.exists(created[0])


@NEEDS_BINARY
def test_check_can_keep_its_scratch_directory(capsys, monkeypatch):
    """--keep 留下临时目录并说出它在哪（复现一个失败时要进去看）；--only 只跑名字对得上的
    检查，总结行如实标注这是个子集。"""
    created = recording_scratch(monkeypatch)
    try:
        assert wire.main(["check", "--keep", "--only", "self-test"]) == 0
    finally:
        for path in created:
            if os.path.isdir(path):
                shutil.rmtree(path)
    lines = capsys.readouterr().out.splitlines()
    kept = re.match(r"^\[note\] scratch kept: (.+)$", lines[-2])
    assert kept and kept.group(1) == created[0]
    assert re.match(r"^\[PASS\] e2e_anthropic_wire: 1 passed, 0 failed, 0 known gap\(s\) \(subset: --only self-test\) — [\d.]+ s$",
                    lines[-1]), lines[-1]


@NEEDS_BINARY
def test_verbose_and_only_narrow_the_run_and_say_so(capsys, monkeypatch):
    """--verbose 给出逐请求的明细行（缩进四格）；别的场景一个字都不提。"""
    monkeypatch.setattr(wire, "STRICT", False)
    assert wire.main(["check", "--verbose", "--only", "compression"]) == 0
    lines = capsys.readouterr().out.splitlines()
    assert "(subset: --only compression)" in lines[-1]
    assert any(line.startswith("    ") for line in lines)
    assert "[PASS] self-test: mutations — %s" % wire.SELF_TEST_OK in lines          # 自测不受 --only 影响
    assert not any(line.startswith("[") and " shapes:" in line for line in lines)
    assert summary_of("\n".join(lines))[0] == "PASS"


def test_verbose_lists_every_problem_of_a_gap(capsys, monkeypatch):
    """--verbose 承诺「每一条问题」：缺口折叠起来的那些（安静模式是 (+N more)）逐条缩进列出；
    安静模式仍然一项检查一行，单条问题的缺口不提 more。"""
    file_synthetic_gap(monkeypatch)
    problems = [("lifetime", "problem one"), ("lifetime", "problem two"), ("lifetime", "problem three")]
    rep = wire.Reporter(verbose=True, only=None, strict=False)
    assert rep.emit("sc: request shape", problems, "never shown", gaps=("lifetime",))
    lines = capsys.readouterr().out.splitlines()
    assert lines == ["[GAP] sc: request shape — KNOWN KERNEL GAP — reported, not counted (--strict makes it fatal): "
                     "problem one — %s" % wire.KNOWN_GAPS["lifetime"], "    problem two", "    problem three"]
    rep = wire.Reporter(verbose=False, only=None, strict=False)
    assert rep.emit("sc: request shape", problems, "never shown", gaps=("lifetime",))
    lines = capsys.readouterr().out.splitlines()
    assert len(lines) == 1 and "(+2 more, --verbose shows all)" in lines[0] and "problem two" not in lines[0]
    assert rep.emit("sc: single", problems[:1], "never shown", gaps=("lifetime",))
    assert "(+1 more" not in capsys.readouterr().out


@NEEDS_BINARY
def test_verbose_hides_no_problem_of_a_gap(capsys, monkeypatch):
    """安静模式折进 (+N more) 的缺口问题，--verbose 里一条不少：multi-turn 的缺口行后面紧跟 N 行缩进
    （这个场景的明细行都在检查行之前，所以数得出确切的行数）。缺口是合成的（file_synthetic_gap）。"""
    file_synthetic_gap(monkeypatch)
    assert wire.main(["check", "--only", "multi-turn"]) == 0
    hidden = {}
    for line in capsys.readouterr().out.splitlines():
        found = re.match(r"^\[GAP\] (.+?) — KNOWN KERNEL GAP.*\(\+(\d+) more, --verbose shows all\)", line)
        if found:
            hidden[found.group(1)] = int(found.group(2))
    assert hidden == {"multi-turn: request shape": 2, "multi-turn: new user turn ends the carry": 2}, hidden
    assert wire.main(["check", "--verbose", "--only", "multi-turn"]) == 0
    lines = capsys.readouterr().out.splitlines()
    for name, count in hidden.items():
        start = next(i for i, line in enumerate(lines) if line.startswith("[GAP] %s — " % name))
        assert "(+%d more" % count not in lines[start], lines[start]
        assert all(line.startswith("    ") for line in lines[start + 1:start + 1 + count]), lines[start + 1:]
        assert lines[start + 1 + count].startswith("["), lines[start + 1 + count]


@NEEDS_BINARY
def test_strict_turns_a_known_gap_into_a_failure(capsys, monkeypatch):
    """--strict（与 TA_WIRE_STRICT=1 同效）：已登记的缺口从 [GAP] 变成 [FAIL]，退出码 1。
    缺口是合成的（file_synthetic_gap）。"""
    file_synthetic_gap(monkeypatch)
    assert wire.main(["check", "--only", "multi-turn"]) == 0
    gaps = [line for line in capsys.readouterr().out.splitlines() if line.startswith("[GAP] ")]
    assert len(gaps) == 2, gaps
    for strict in (["--strict"], []):
        monkeypatch.setattr(wire, "STRICT", not strict)
        assert wire.main(["check", "--only", "multi-turn"] + strict) == 1
        lines = capsys.readouterr().out.splitlines()
        failed = [line for line in lines if line.startswith("[FAIL] ")]
        assert not any(line.startswith("[GAP] ") for line in lines)
        assert len(failed) == len(gaps) + 1 and failed[-1].startswith("[FAIL] e2e_anthropic_wire: %d failed (" % len(gaps))


@NEEDS_BINARY
def test_a_filed_gap_that_stopped_reproducing_fails_the_check(capsys, monkeypatch):
    """strict-xfail：KNOWN_GAPS 里登记着一条已不再复现的缺口 → check 必须红，
    提醒把它从表里撤掉（否则这张表就成了修好的 bug 被遗忘的地方）。"""
    monkeypatch.setattr(wire, "STRICT", False)
    monkeypatch.setattr(wire, "KNOWN_GAPS", dict(wire.KNOWN_GAPS, bogus="never reproduces"))
    assert wire.main(["check"]) == 1
    lines = capsys.readouterr().out.splitlines()
    failed = [line for line in lines if line.startswith("[FAIL] ")]
    assert failed == ["[FAIL] known gap: bogus — it no longer reproduces — retire it from KNOWN_GAPS",
                      failed[-1]]
    assert failed[-1].startswith("[FAIL] e2e_anthropic_wire: 1 failed (known gap: bogus), ")


def test_check_fails_loudly_without_a_binary(capsys, monkeypatch):
    """没有二进制 = 红，不是 skip（CI 里它必须是被听见的那一声）：setup 一条 [FAIL]、
    说清怎么构建、总结行说 0 passed。"""
    monkeypatch.setattr(wire, "BIN", os.path.join(tempfile.gettempdir(), "ta-wire-no-such-binary"))
    assert wire.main(["check"]) == 1
    lines = capsys.readouterr().out.splitlines()
    assert lines[0].startswith("[FAIL] setup: binary — binary not found or not executable: ")
    assert "cargo build --locked --features code" in lines[0] and "nothing was checked" in lines[0]
    assert summary_of("\n".join(lines)) == ("FAIL", 0, 1, 0, "setup: binary")


# ── the command line, the way CI calls it (a fresh interpreter) ────────────────


def cli(*argv, **env):
    """本脚本经命令行跑一次：返回 (退出码, 标准输出)。"""
    environment = dict(os.environ, PYTHONDONTWRITEBYTECODE="1", **env)
    proc = subprocess.run([sys.executable, str(SCRIPT)] + list(argv), env=environment,
                          capture_output=True, text=True, timeout=120)
    return proc.returncode, proc.stdout, proc.stderr


def test_the_command_line_introduces_its_three_subcommands():
    code, out, _ = cli("--help")
    assert code == 0
    for word in ("{check,capture,audit}", "offline", "manual"):
        assert word in out, word
    code, out, _ = cli("capture", "--help")
    assert code == 0 and all(flag in out for flag in ("--upstream", "--listen", "--out", "--upstream-timeout"))
    code, out, _ = cli("audit", "--help")
    assert code == 0 and all(flag in out for flag in ("--session", "--result", "--carry-off", "--verbose", "--only",
                                                      "--strict"))
    code, out, _ = cli("check", "--help")
    assert code == 0 and all(flag in out for flag in ("--verbose", "--only", "--strict", "--keep"))


def test_the_command_line_fails_the_same_way_without_a_binary(tmp_path):
    """CI 的调用方式（不带参数、TA_BIN 缺省或指错）也是 exit 1，第一行就是原因。"""
    for argv in ((), ("check",)):
        code, out, _ = cli(*argv, TA_BIN=str(tmp_path / "no-such-binary"))
        assert code == 1, argv
        assert out.startswith("[FAIL] setup: binary — binary not found or not executable: "), argv
        assert "nothing was checked" in out and "known gap(s)" not in out.splitlines()[0]


def test_the_command_line_passes_the_audit_exit_code_through(tmp_path):
    code, out, _ = cli("audit", captured(tmp_path))
    assert code == 0 and out.splitlines()[-1].startswith("[PASS] audit_anthropic_wire: "), out
    edit_capture(str(tmp_path / "cap"), "requests.jsonl",
                 lambda records: records[1]["body"]["messages"][1]["content"].pop(0))      # 回传的块被拿掉
    code, out, _ = cli("audit", str(tmp_path / "cap"))
    assert code == 1 and "[FAIL] audit: echo fidelity" in out and "[FAIL] audit_anthropic_wire: " in out


def test_the_parser_refuses_an_incomplete_command(monkeypatch):
    """不带子命令 = check；每个子命令该有的参数缺了就是用法错误（exit 2），不会去建文件。"""
    seen = []
    monkeypatch.setattr(wire, "cmd_check", lambda args: seen.append(vars(args)) or 0)
    assert wire.main([]) == 0 and wire.main(["--verbose", "--only", "fold"]) == 0
    assert seen == [{"verbose": False, "only": None, "strict": False, "keep": False, "command": "check"},
                    {"verbose": True, "only": "fold", "strict": False, "keep": False, "command": "check"}]
    for argv in (["capture", "--out", "x"],                        # 没说上游
                 ["capture", "--upstream", "http://127.0.0.1:9"],  # 没说记到哪
                 ["audit"],                                        # 没给 capture 目录
                 ["audit", "/nonexistent", "--carry-on"],          # 没有这个开关
                 ["cheque"]):                                      # 没有这个子命令
        with pytest.raises(SystemExit) as caught:
            wire.main(argv)
        assert caught.value.code == 2, argv


# ── wiring drift guards: the gate, the workflow and both READMEs agree ─────────

WIRE_COMMAND = "python3 scripts/e2e_anthropic_wire.py"
SCENARIOS_COMMAND = "python3 scripts/e2e_scenarios.py"
REGRESS_COMMAND = "python3 scripts/regress_eval.py check"
WIRE_FLAGS = ("--upstream", "--out", "--session", "--result", "--carry-off", "--strict")


def commands_of_shell_group(shell, name):
    """check.sh 的 `name()` 组里依次执行的命令（run_check 的标签与续行符不算）。"""
    group = re.search(r"^%s\(\) \{\n(.*?)(?=^\})" % re.escape(name), shell, re.M | re.S)
    assert group, name
    out = []
    for line in group.group(1).splitlines():
        line = line.strip()
        if line and not line.startswith(("#", "run_check ")):
            out.append(line.rstrip("\\").strip())
    return out


def commands_of_workflow_job(workflow, name):
    """ci.yml 的 `name` job 里依次 `run:` 的单行命令。"""
    job = re.search(r"^  %s:\n(.*?)(?=^  \S|\Z)" % re.escape(name), workflow, re.M | re.S)
    assert job, name
    return [found.group(1).strip() for found in re.finditer(r"^\s*- run: ([^\n]+)$", job.group(1), re.M)]


def test_check_is_wired_into_the_gate_and_the_workflow_in_the_same_place():
    """接线漂移守卫（CLAUDE.md：命令改了要同步 check.sh 与 ci.yml 两边，两组「逐字对齐」）：
    check.sh 的 e2e 组与 ci.yml 的 e2e job 执行同一串命令；本脚本在两处都恰有一条，
    都排在 e2e_scenarios.py 之后、回归任务集自检之前，上面各有一段说明。"""
    shell = (ROOT / "scripts" / "check.sh").read_text(encoding="utf-8")
    workflow = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    commands = commands_of_shell_group(shell, "group_e2e")
    assert commands == commands_of_workflow_job(workflow, "e2e"), (commands, commands_of_workflow_job(workflow, "e2e"))
    assert commands.count(WIRE_COMMAND) == 1 and shell.count(WIRE_COMMAND) == workflow.count(WIRE_COMMAND) == 1
    assert commands[commands.index(SCENARIOS_COMMAND) + 1] == WIRE_COMMAND
    assert commands.index(WIRE_COMMAND) < commands.index(REGRESS_COMMAND)
    assert re.search(r"(?:^\s*#[^\n]*\n)+^\s*run_check \"e2e: [a-z -]+\" \\\n\s*" + re.escape(WIRE_COMMAND) + r"$",
                     shell, re.M), "the step in check.sh wants a comment and a run_check label"
    assert re.search(r"(?:^\s*#[^\n]*\n)+^\s*- run: " + re.escape(WIRE_COMMAND) + r"$", workflow, re.M), \
        "the step in ci.yml wants a comment"
    # check.sh 开头的用法注释也要说到它：e2e 组不再是「场景 + 回归任务集」两件事。
    assert re.search(r"^#   \./scripts/check\.sh e2e[^\n]*Anthropic[^\n]*$", shell, re.M)


def wire_section(text):
    """README 里标题带本脚本名的 `###` 小节正文（到下一个 `###` 为止）。"""
    found = re.search(r"^### [^\n]*e2e_anthropic_wire\.py[^\n]*\n(.*?)(?=^### |\Z)", text, re.M | re.S)
    assert found, "no ### section about e2e_anthropic_wire.py"
    return found.group(1)


def test_both_readmes_tell_the_same_story():
    """两份 README 平行（CLAUDE.md：改了共享事实要两边同步）：各有一小节讲同一个脚本，
    示例命令逐字相同（三个子命令都有一句），同一组开关都讲到了，
    都说清 check 是 CI 门禁、capture / audit 是手动通道。"""
    english = wire_section((ROOT / "README.md").read_text(encoding="utf-8"))
    chinese = wire_section((ROOT / "README.zh-CN.md").read_text(encoding="utf-8"))
    examples = [[line.strip().split("#", 1)[0].strip() for line in body.splitlines()
                 if line.strip().startswith(WIRE_COMMAND)] for body in (english, chinese)]
    assert len(examples[0]) >= 3 and examples[0] == examples[1], examples
    assert {example.split()[2] for example in examples[0]} == {"check", "capture", "audit"}
    for body in (english, chinese):
        assert all(re.search(re.escape(flag) + r"(?![\w-])", body) for flag in WIRE_FLAGS), body
        assert "check.sh e2e" in body and body.count("e2e_anthropic_wire.py") == english.count("e2e_anthropic_wire.py")
    assert "CI gate" in english and "CI 门禁" in chinese


# ── hygiene guard over the two files of this change ───────────────────────────


@pytest.mark.parametrize("path", [SCRIPT, Path(__file__).resolve()], ids=["script", "tests"])
def test_no_key_pattern_and_no_address_beyond_loopback(path):
    """卫生守卫：这两个文件里没有密钥样字面量（假凭据一律运行时拼）；每一个 IP 都是回环；
    每一个 URL 的主机名都是回环或占位符——私网端点地址永不进仓库。"""
    text = path.read_text(encoding="utf-8")
    assert wire.find_secrets(text) == []
    assert set(re.findall(r"\b(?:\d{1,3}\.){3}\d{1,3}\b", text)) <= {"127.0.0.1"}
    for url in re.findall(r"https?://[^\s\"'<>)\[\]`,]+", text):
        assert urlsplit(url.rstrip(".:")).hostname in ("127.0.0.1", "localhost", "host", "placeholder.invalid", "%s"), url


