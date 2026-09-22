#[cfg(test)]
use super::SseReader;
use super::{
    Block, CancellableSse, ChatEvent, ChatRequest, ChatResponse, Message, Provider, Role,
    SseSource, StreamAbort, ToolDef, Usage,
};
use anyhow::{Context, Result};
use serde_json::{json, Value};

pub struct Anthropic {
    client: reqwest::blocking::Client,
    base_url: String,
    api_key: String,
    idle: std::time::Duration,
}

impl Anthropic {
    pub fn new(
        client: reqwest::blocking::Client,
        base_url: String,
        api_key: &str,
        idle: std::time::Duration,
    ) -> Self {
        Self {
            client,
            base_url,
            api_key: api_key.to_string(),
            idle,
        }
    }

    fn message_json(m: &Message) -> Value {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let content: Vec<Value> = m
            .content
            .iter()
            .map(|b| match b {
                Block::Text(t) => json!({"type": "text", "text": t}),
                Block::Image { media_type, data } => json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": media_type, "data": data}
                }),
                // Anthropic's `document` block (PDF support): same base64
                // source shape as images; media_type is "application/pdf".
                // Only base64 is wired — the url / Files-API `file_id` sources
                // are host-side concerns (download and push the bytes).
                Block::Document { media_type, data } => json!({
                    "type": "document",
                    "source": {"type": "base64", "media_type": media_type, "data": data}
                }),
                Block::ToolUse { id, name, input } => json!({
                    "type": "tool_use", "id": id, "name": name, "input": input
                }),
                Block::ToolResult {
                    tool_use_id,
                    content,
                    image,
                    is_error,
                } => {
                    // Anthropic rejects EMPTY text blocks with 400, so the text
                    // part is only emitted when non-empty: an image-bearing
                    // result with empty text sends just [image]; a result with
                    // neither text nor image omits `content` entirely (the API
                    // allows a content-less tool_result) instead of `"text":""`.
                    let body = match image.as_deref() {
                        Some(Block::Image { media_type, data }) => {
                            let img = json!({
                                "type": "image",
                                "source": {"type": "base64", "media_type": media_type, "data": data}
                            });
                            if content.is_empty() {
                                json!([img])
                            } else {
                                json!([img, {"type": "text", "text": content}])
                            }
                        }
                        _ if content.is_empty() => serde_json::Value::Null,
                        _ => json!(content),
                    };
                    let mut v = json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "is_error": is_error
                    });
                    if !body.is_null() {
                        v["content"] = body;
                    }
                    v
                }
            })
            .collect();
        json!({"role": role, "content": content})
    }

    fn tools_json(tools: &[ToolDef]) -> Value {
        let list: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.schema
                })
            })
            .collect();
        json!(list)
    }
}

/// Partially-assembled block while streaming.
enum Pending {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
    /// A `thinking` content block. Buffered only to coalesce its deltas into
    /// ThinkingDelta events; finalize drops it (thinking never enters history).
    Thinking(String),
}

/// Finalize a pending block into a concrete Block, tolerant of incomplete
/// JSON. Shared by the content_block_stop handler and the truncation-recovery
/// flushes (content_block_start overwrite + end-of-stream). Half-delivered
/// tool arguments become {"_raw": ...} instead of failing: a truncated stream
/// surfaces as a stream_truncated stop_reason, and a model's malformed JSON
/// on a COMPLETE stream reaches the tool layer (which reports the bad input
/// back to the model) instead of killing the whole run.
fn finalize_pending_lenient(p: Pending) -> Option<Block> {
    match p {
        // Skip an empty text block: a content_block_start with no later delta
        // (stream cut right after the block opened) would otherwise solidify a
        // Block::Text("") into the history, and the next turn's request is
        // rejected by Anthropic with 400. OpenAI's parser guards with
        // !text.is_empty(); this mirrors it.
        Pending::Text(t) if t.is_empty() => None,
        Pending::Text(t) => Some(Block::Text(t)),
        Pending::ToolUse { id, name, json } => {
            let input = if json.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&json).unwrap_or_else(|_| json!({"_raw": json}))
            };
            Some(Block::ToolUse { id, name, input })
        }
        // Thinking is event-only: its deltas already went out as ThinkingDelta.
        // It never solidifies into a history block (no major API asks thinking
        // to be echoed back on the next turn).
        Pending::Thinking(_) => None,
    }
}

impl Provider for Anthropic {
    fn chat(&self, req: &ChatRequest, on_event: &mut dyn FnMut(ChatEvent)) -> Result<ChatResponse> {
        let body = Self::build_body(req);
        let mut rb = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json");
        if let Some(t) = req.per_request_timeout {
            rb = rb.timeout(t);
        }
        let resp = rb
            .json(&body)
            .send()
            .context("request to Anthropic API failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            // Structured status + historical display text (see ApiStatusError).
            return Err(super::ApiStatusError::new(
                status.as_u16(),
                format!("Anthropic API error {status}: {text}"),
            )
            .into());
        }

        let mut sse = CancellableSse::new(resp, req.cancel.clone(), self.idle)?;
        Self::parse_loop(&mut sse, on_event)
    }
}

impl Anthropic {
    /// Assemble the `/v1/messages` request body. Pure (no `&self`, no I/O) so
    /// the prompt-cache gating is directly unit-testable (see
    /// `tests::build_body_*`). Prompt caching marks up to three ephemeral
    /// breakpoints when `req.cache` is set, so a multi-turn run re-reads the
    /// stable prefix cheaply instead of re-billing it every turn. One-off
    /// requests (the compression summary) pass `cache=false` to skip the 1.25×
    /// write premium on content that's never re-read. Anthropic ignores
    /// breakpoints below its ~1024-token floor (no error, no charge), so this
    /// is always safe to apply, and ≤3 breakpoints stays under the 4-breakpoint
    /// cap. OpenAI caching is implicit (no body field), so this markup is
    /// Anthropic-only by virtue of living here.
    fn build_body(req: &ChatRequest) -> Value {
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "stream": true,
            "messages": req.messages.iter().map(Self::message_json).collect::<Vec<_>>(),
        });
        // Adaptive thinking (Claude 4.6+). The effort string is forwarded
        // verbatim — NOT mapped to an enum, since Anthropic's own ladder
        // (low/medium/high/max) shifts across generations and the old
        // budget_tokens path was removed in Opus 4.7 (sending it 400s). "none"
        // disables thinking; any other value enables adaptive + tags effort at
        // the top level; None leaves the server default. Field shape follows
        // the 2026-08 adaptive-thinking API — adjust here if the API moves.
        match req.reasoning_effort {
            Some("none") => {
                body["thinking"] = json!({"type": "disabled"});
            }
            Some(eff) => {
                body["thinking"] = json!({"type": "adaptive"});
                // effort lives in a SEPARATE top-level `output_config` object —
                // NOT inside `thinking` and NOT as a bare top-level field. This
                // is the API's #1 footgun (per the official docs + AWS Bedrock):
                // a bare `effort` is silently ignored, so the knob would appear
                // to work but have no effect.
                body["output_config"] = json!({"effort": eff});
            }
            None => {}
        }
        // Per-request sampling temperature (LD-4): forwarded only when the
        // caller set it — the agent's turns never do, so their bodies are
        // byte-identical to before. Anthropic's range is 0..=1; the in-tree
        // caller sends 0.0. A caller out of range gets the API's own 400
        // (diagnosed like any other request error), not a silent clamp.
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(sys) = req.system {
            if req.cache {
                // System must be a content-block array (not a bare string) to
                // carry cache_control; tag it so the prompt is cached across turns.
                body["system"] = json!([{
                    "type": "text",
                    "text": sys,
                    "cache_control": {"type": "ephemeral"},
                }]);
            } else {
                body["system"] = json!(sys);
            }
        }
        if !req.tools.is_empty() {
            body["tools"] = Self::tools_json(req.tools);
            if req.cache {
                // The tool definitions are stable for the whole run — cache
                // them by tagging the last one.
                if let Some(last) = body["tools"].as_array_mut().and_then(|a| a.last_mut()) {
                    last["cache_control"] = json!({"type": "ephemeral"});
                }
            }
        }
        if req.cache {
            // Cache the conversation prefix: tag the last content block of the
            // last message, so on the next turn everything before it is a read.
            if let Some(last_block) = body["messages"]
                .as_array_mut()
                .and_then(|msgs| msgs.last_mut())
                .and_then(|m| m["content"].as_array_mut())
                .and_then(|blocks| blocks.last_mut())
            {
                last_block["cache_control"] = json!({"type": "ephemeral"});
            }
        }
        body
    }

    /// Assemble a full response from an SSE byte stream (sync, for tests).
    #[cfg(test)]
    fn parse_stream<R: std::io::Read>(
        reader: R,
        on_event: &mut dyn FnMut(ChatEvent),
    ) -> Result<ChatResponse> {
        let mut sse = SseReader::new(reader);
        Self::parse_loop(&mut sse, on_event)
    }

    /// Streaming core shared by the sync test path and the live path
    /// (`CancellableSse`). A cancel/idle abort surfaces as a stop_reason
    /// carrying whatever partial blocks arrived — mirroring stream_truncated,
    /// so the agent ends the run cleanly instead of hanging on a half-open
    /// connection.
    fn parse_loop<S: SseSource>(
        src: &mut S,
        on_event: &mut dyn FnMut(ChatEvent),
    ) -> Result<ChatResponse> {
        let mut blocks: Vec<Block> = Vec::new();
        let mut pending: Option<Pending> = None;
        let mut stop_reason = String::from("end_turn");
        let mut got_stop = false;
        let mut abort: Option<&'static str> = None;
        let mut usage = Usage::default();
        // Distinguishes "gateway reported zero" from "gateway reported
        // nothing": without a usage frame at all the response must carry
        // usage=None, not Some(all-zeros) — downstream accounting keys on
        // Some/None to tell "0 tokens" from "usage unknown".
        let mut saw_usage = false;

        loop {
            let data = match src.next_payload() {
                Ok(Some(d)) => d,
                Ok(None) => break, // end of stream
                Err(StreamAbort::Cancelled) => {
                    abort = Some("cancelled");
                    break;
                }
                Err(StreamAbort::IdleTimeout) => {
                    abort = Some("stream_idle");
                    break;
                }
                // A mid-stream read failure must NOT be treated as a clean
                // end-of-stream — that would assemble a partial response and
                // report it as success. Return with stop_reason "stream_error"
                // (carrying the message as a Text block + any usage accumulated
                // so far, e.g. input/cache tokens from message_start) so the
                // agent bills the partial cost honestly and maps this to an Err
                // that triggers retry — bailing here would drop the usage that
                // Anthropic already billed for this turn.
                Err(StreamAbort::StreamError(msg)) => {
                    blocks.push(Block::Text(format!(
                        "[stream error] Anthropic stream read error: {msg}"
                    )));
                    return Ok(ChatResponse {
                        blocks,
                        stop_reason: "stream_error".into(),
                        usage: saw_usage.then_some(usage),
                    });
                }
            };
            let ev: Value = match serde_json::from_str(&data) {
                Ok(v) => v,
                Err(_) => continue, // ignore unparseable keep-alives
            };
            match ev["type"].as_str().unwrap_or("") {
                "message_start" => {
                    let u = &ev["message"]["usage"];
                    if u.is_object() {
                        saw_usage = true;
                    }
                    if let Some(i) = u["input_tokens"].as_u64() {
                        usage.input_tokens = i;
                    }
                    if let Some(c) = u["cache_creation_input_tokens"].as_u64() {
                        usage.cache_creation_input_tokens = c;
                    }
                    if let Some(r) = u["cache_read_input_tokens"].as_u64() {
                        usage.cache_read_input_tokens = r;
                    }
                }
                "content_block_start" => {
                    // A previous block that never received content_block_stop
                    // (truncated stream) would be overwritten here — flush it
                    // leniently (its tool JSON may be half-delivered).
                    if let Some(p) = pending.take() {
                        if let Some(b) = finalize_pending_lenient(p) {
                            blocks.push(b);
                        }
                    }
                    let cb = &ev["content_block"];
                    pending = Some(match cb["type"].as_str().unwrap_or("") {
                        "tool_use" => Pending::ToolUse {
                            id: cb["id"].as_str().unwrap_or_default().to_string(),
                            name: cb["name"].as_str().unwrap_or_default().to_string(),
                            // The official API always sends `"input": {}` here
                            // and streams the real arguments via later
                            // input_json_delta events; a non-conforming gateway
                            // may inline the full object in content_block_start
                            // instead. Seed the buffer with it iff it's a
                            // NON-EMPTY object — official streams (empty
                            // object) are byte-for-byte unaffected.
                            json: cb["input"]
                                .as_object()
                                .filter(|o| !o.is_empty())
                                .map(|o| Value::Object(o.clone()).to_string())
                                .unwrap_or_default(),
                        },
                        // Extended-thinking blocks. `thinking` streams its text
                        // via later thinking_delta events (surfaced as
                        // ThinkingDelta); `redacted_thinking` carries no readable
                        // text (only a signature), so it stays a silent
                        // placeholder. Neither enters history — finalize drops
                        // Pending::Thinking.
                        "thinking" | "redacted_thinking" => Pending::Thinking(String::new()),
                        _ => Pending::Text(String::new()),
                    });
                }
                "content_block_delta" => {
                    let delta = &ev["delta"];
                    match delta["type"].as_str().unwrap_or("") {
                        "text_delta" => {
                            let t = delta["text"].as_str().unwrap_or_default();
                            // Only emit the UI event when the text actually
                            // lands in a pending block: a stray text_delta
                            // with no open text block (out-of-order stream)
                            // would otherwise show on screen but be missing
                            // from the assembled history.
                            if let Some(Pending::Text(buf)) = &mut pending {
                                buf.push_str(t);
                                on_event(ChatEvent::TextDelta(t.to_string()));
                            }
                        }
                        "input_json_delta" => {
                            if let Some(Pending::ToolUse { id, name, json }) = &mut pending {
                                let frag = delta["partial_json"].as_str().unwrap_or_default();
                                // Display feed only (mirrors the OpenAI
                                // provider): the assembled input lands in the
                                // ToolUse block; this lets a UI render the
                                // arguments as they stream in.
                                if !frag.is_empty() {
                                    on_event(ChatEvent::ToolInputDelta {
                                        id: id.clone(),
                                        name: name.clone(),
                                        delta: frag.to_string(),
                                    });
                                }
                                json.push_str(frag);
                            }
                        }
                        // Anthropic extended-thinking streaming: each delta
                        // carries a `thinking` text fragment. Surface it as
                        // ThinkingDelta (event-only); the buffer is kept solely
                        // so a later non-thinking delta can't misattach to it.
                        // `signature_delta` (the thinking block's integrity tag)
                        // falls through to `_` — we never echo thinking back, so
                        // the signature is irrelevant to us.
                        "thinking_delta" => {
                            if let Some(Pending::Thinking(buf)) = &mut pending {
                                let t = delta["thinking"].as_str().unwrap_or_default();
                                if !t.is_empty() {
                                    buf.push_str(t);
                                    on_event(ChatEvent::ThinkingDelta(t.to_string()));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    // Lenient here too, not just on the truncated-stream flush
                    // paths: a model that emits malformed tool JSON on an
                    // otherwise complete stream gets a {"_raw": ...} input the
                    // tool layer can reject back to the model (self-heal),
                    // matching the OpenAI provider — hard-failing the whole run
                    // on a single model-level JSON slip was needlessly fatal.
                    if let Some(p) = pending.take() {
                        if let Some(b) = finalize_pending_lenient(p) {
                            blocks.push(b);
                        }
                    }
                }
                "message_delta" => {
                    if let Some(r) = ev["delta"]["stop_reason"].as_str() {
                        stop_reason = r.to_string();
                        got_stop = true;
                    }
                    if let Some(o) = ev["usage"]["output_tokens"].as_u64() {
                        usage.output_tokens = o;
                        saw_usage = true;
                    }
                    // Some Anthropic-protocol gateways report input (and
                    // cache) tokens ONLY here — message_start carries zeros
                    // (real-journey A/B on a glm relay found input billed as
                    // 0, breaking the honest-cost contract). Fold with max so
                    // a real-API run that already counted them at
                    // message_start never double-counts.
                    if let Some(i) = ev["usage"]["input_tokens"].as_u64() {
                        usage.input_tokens = usage.input_tokens.max(i);
                        saw_usage = saw_usage || i > 0;
                    }
                    for (cur, key) in [
                        (
                            &mut usage.cache_creation_input_tokens,
                            "cache_creation_input_tokens",
                        ),
                        (
                            &mut usage.cache_read_input_tokens,
                            "cache_read_input_tokens",
                        ),
                    ] {
                        if let Some(v) = ev["usage"][key].as_u64() {
                            *cur = (*cur).max(v);
                        }
                    }
                }
                "error" => {
                    // Return with stop_reason "stream_error" carrying the
                    // message + any usage already accumulated (message_start's
                    // input/cache tokens are billed by Anthropic even when the
                    // stream errors mid-generation). Bailing here would drop
                    // that usage, violating the honest-cost contract (CLAUDE.md
                    // "重试诚实成本"). The agent maps "stream_error" to an Err
                    // that triggers retry while billing the partial cost.
                    let msg = ev["error"]["message"].as_str().unwrap_or("(no message)");
                    blocks.push(Block::Text(format!(
                        "[stream error] Anthropic stream error: {msg}"
                    )));
                    return Ok(ChatResponse {
                        blocks,
                        stop_reason: "stream_error".into(),
                        usage: saw_usage.then_some(usage),
                    });
                }
                // The official API closes the body right after message_stop,
                // so the loop could previously rely on EOF alone to end. A
                // gateway that HOLDS the connection open (LiteLLM-style
                // proxies, enterprise relays) never EOFs: the loop sat until
                // the 30s idle timeout, and that abort ("stream_idle")
                // overrode the real stop_reason — an entire tool_use turn was
                // then killed by the agent's aborted-branch block stripping.
                // Breaking here mirrors the OpenAI parser's explicit
                // `data: [DONE]` termination; the pending flush and the
                // stop_reason priority chain below are shared with the EOF
                // path, so nothing else changes.
                "message_stop" => break,
                _ => {}
            }
        }

        // A stream cut off mid-block (network drop, cancel, or idle timeout)
        // leaves a pending block unflushed. Flush it leniently (half-delivered
        // tool JSON becomes _raw) rather than dropping the block or
        // hard-failing on a parse error.
        if let Some(p) = pending.take() {
            if let Some(b) = finalize_pending_lenient(p) {
                blocks.push(b);
            }
        }
        // stop_reason priority: explicit abort (cancel/idle) > real
        // message_delta > stream_truncated. Without this an incomplete stream
        // would masquerade as a clean end_turn and break batch/CI checks.
        let stop_reason = if let Some(r) = abort {
            r.into()
        } else if got_stop {
            stop_reason
        } else {
            "stream_truncated".into()
        };
        Ok(ChatResponse {
            blocks,
            stop_reason,
            usage: saw_usage.then_some(usage),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &[u8]) -> ChatResponse {
        Anthropic::parse_stream(raw, &mut |_| {}).unwrap()
    }

    /// Real-journey A/B finding: some Anthropic-protocol gateways (a glm
    /// relay) zero message_start.usage and report input/cache tokens ONLY in
    /// message_delta.usage — parse must fold them or every run bills input
    /// as 0 (honest-cost contract). max-fold keeps a real-API stream (which
    /// counts input at message_start and may repeat it in the delta) from
    /// double-counting.
    #[test]
    fn usage_input_only_in_message_delta_is_counted() {
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"input_tokens\":14,\"output_tokens\":32,\"cache_read_input_tokens\":6}}\n\n",
        );
        let resp = parse(raw.as_bytes());
        let u = resp.usage.expect("delta-only usage must surface as Some");
        assert_eq!((u.input_tokens, u.output_tokens), (14, 32));
        assert_eq!(u.cache_read_input_tokens, 6);
        // No double count: message_start already billed, delta repeats it.
        let raw2 = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1500,\"cache_read_input_tokens\":800}}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":1500,\"output_tokens\":9}}\n\n",
        );
        let u2 = parse(raw2.as_bytes()).usage.unwrap();
        assert_eq!(
            (
                u2.input_tokens,
                u2.cache_read_input_tokens,
                u2.output_tokens
            ),
            (1500, 800, 9)
        );
    }

    #[test]
    fn stream_text_and_stop_reason() {
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
        );
        let resp = parse(raw.as_bytes());
        assert_eq!(resp.stop_reason, "end_turn");
        assert_eq!(resp.blocks.len(), 1);
        assert!(matches!(&resp.blocks[0], Block::Text(t) if t == "Hello world"));
        assert_eq!(
            resp.usage.map(|u| (u.input_tokens, u.output_tokens)),
            Some((12, 7))
        );
    }

    #[test]
    fn stream_without_usage_frames_reports_none() {
        // A gateway that never sends usage frames must surface usage=None,
        // not Some(all-zeros) — downstream accounting keys on Some/None to
        // tell "0 tokens billed" from "usage unknown" (honest-cost contract).
        let raw = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        );
        let resp = parse(raw.as_bytes());
        assert_eq!(resp.stop_reason, "end_turn");
        assert!(matches!(&resp.blocks[0], Block::Text(t) if t == "hi"));
        assert!(resp.usage.is_none());
    }

    #[test]
    fn thinking_block_surfaces_event_and_stays_out_of_history() {
        // Extended thinking: a `thinking` content block streams its text via
        // thinking_delta. The trace surfaces as ThinkingDelta events AND never
        // enters the assembled blocks (no major API asks thinking echoed back).
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"reasoning here\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        );
        let mut thinking = String::new();
        let resp = Anthropic::parse_stream(raw.as_bytes(), &mut |e| {
            if let crate::provider::ChatEvent::ThinkingDelta(t) = e {
                thinking.push_str(&t);
            }
        })
        .unwrap();
        assert_eq!(thinking, "reasoning here");
        // Only the text block survives into history; thinking is event-only.
        assert_eq!(resp.blocks.len(), 1);
        assert!(matches!(&resp.blocks[0], Block::Text(t) if t == "ok"));
    }

    #[test]
    fn empty_text_block_is_dropped_not_solidified() {
        // A content_block_start whose text never receives a delta (stream cut
        // right after the block opened) must NOT solidify a Block::Text("")
        // into the history — the next turn's request would be rejected by
        // Anthropic with 400. The empty block is dropped, mirroring the
        // OpenAI parser's !text.is_empty() guard.
        let raw = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        );
        let resp = parse(raw.as_bytes());
        assert_eq!(resp.stop_reason, "end_turn");
        assert!(
            resp.blocks.is_empty(),
            "empty text block must be dropped, got: {:?}",
            resp.blocks
        );
    }

    #[test]
    fn stream_error_event_returns_usage_not_bail() {
        // A mid-stream error event (overloaded / content_filter mid-generation)
        // arrives AFTER message_start already billed input + cache tokens.
        // The honest-cost contract (CLAUDE.md "重试诚实成本") requires that
        // usage to flow back so retry/billing accounts for it — previously the
        // error event bailed, dropping the already-billed usage on the floor.
        // Now it returns stop_reason "stream_error" carrying the usage + a
        // Text block with the message; the agent maps that to a retryable Err.
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1500,\"cache_read_input_tokens\":800}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        );
        let resp = parse(raw.as_bytes());
        assert_eq!(resp.stop_reason, "stream_error");
        // input + cache tokens from message_start survived the error path.
        assert_eq!(
            resp.usage
                .map(|u| (u.input_tokens, u.cache_read_input_tokens)),
            Some((1500, 800))
        );
        // The error message is carried in a Text block (prefixed so the agent
        // can extract it when constructing the retryable Err).
        let msg = resp.blocks.iter().find_map(|b| match b {
            Block::Text(t) if t.starts_with("[stream error] ") => Some(t.as_str()),
            _ => None,
        });
        assert!(
            msg.is_some(),
            "missing [stream error] block: {:?}",
            resp.blocks
        );
        assert!(msg.unwrap().contains("Overloaded"));
    }

    #[test]
    fn stream_usage_reads_cache_fields() {
        // When prompt caching engaged, message_start.usage carries
        // cache_creation_input_tokens / cache_read_input_tokens. parse_loop must
        // surface them on Usage (previously they were dropped).
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":300,\"cache_read_input_tokens\":1500}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"hi\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\n",
        );
        let resp = parse(raw.as_bytes());
        let u = resp.usage.expect("anthropic always reports usage");
        assert_eq!(u.input_tokens, 12);
        assert_eq!(u.cache_creation_input_tokens, 300);
        assert_eq!(u.cache_read_input_tokens, 1500);
    }

    #[test]
    fn partial_usage_one_sided_is_reported() {
        // One-sided usage (message_start reported input, stream cut before
        // message_delta's output): the input tokens were billed, so the
        // response must carry Some((9, 0)) — the zero axis reads as
        // "reported zero", distinct from usage=None ("no frame at all",
        // see stream_without_usage_frames_reports_none).
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"hi\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        let resp = parse(raw.as_bytes());
        let u = resp.usage.expect("usage frame seen → Some");
        assert_eq!((u.input_tokens, u.output_tokens), (9, 0));
    }

    #[test]
    fn stream_tool_use_split_json_deltas() {
        // input_json_delta arrives in arbitrary chunks — must reassemble.
        let raw = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"shell\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"comma\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"nd\\\":\\\"ls -la\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
        );
        let resp = parse(raw.as_bytes());
        assert_eq!(resp.stop_reason, "tool_use");
        assert_eq!(resp.blocks.len(), 1);
        match &resp.blocks[0] {
            Block::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "shell");
                assert_eq!(input["command"], "ls -la");
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
    }

    #[test]
    fn stream_tool_input_deltas_carry_block_id_and_name() {
        // partial_json fragments surface as ToolInputDelta tagged with the
        // pending block's id+name (from content_block_start), so a UI can
        // pair fragments across parallel same-name tool calls.
        let raw = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"shell\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"comma\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"nd\\\":\\\"ls -la\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
        );
        let mut events = Vec::new();
        Anthropic::parse_stream(raw.as_bytes(), &mut |e| events.push(e)).unwrap();
        let deltas: Vec<(String, String, String)> = events
            .into_iter()
            .filter_map(|e| match e {
                ChatEvent::ToolInputDelta { id, name, delta } => Some((id, name, delta)),
                _ => None,
            })
            .collect();
        assert_eq!(
            deltas,
            vec![
                ("toolu_1".into(), "shell".into(), "{\"comma".into()),
                ("toolu_1".into(), "shell".into(), "nd\":\"ls -la\"}".into()),
            ]
        );
    }

    #[test]
    fn stream_error_event_returns_stream_error_stop_reason() {
        // Previously this bailed (dropping usage). Now it returns Ok with
        // stop_reason "stream_error" so the agent can bill partial usage and
        // map to a retryable Err. See stream_error_event_returns_usage_not_bail
        // for the full usage-carrying assertion.
        let raw = "data: {\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}\n\n";
        let resp = parse(raw.as_bytes());
        assert_eq!(resp.stop_reason, "stream_error");
        assert!(resp
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Text(t) if t.contains("overloaded"))));
    }

    #[test]
    fn stream_truncated_flushes_pending() {
        // Stream cut off mid-tool-use (no content_block_stop, no message_stop)
        // — the pending tool_use must still be recovered rather than dropped.
        let raw = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_9\",\"name\":\"shell\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"ls\\\"}\"}}\n\n",
        );
        let resp = parse(raw.as_bytes());
        assert_eq!(resp.stop_reason, "stream_truncated");
        assert_eq!(resp.blocks.len(), 1);
        match &resp.blocks[0] {
            Block::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_9");
                assert_eq!(name, "shell");
                assert_eq!(input["command"], "ls");
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
    }

    #[test]
    fn stream_truncated_incomplete_json() {
        // Tool args cut off mid-JSON: must not hard-fail or blame the model —
        // the half-JSON becomes _raw and stop_reason reports the truncation.
        let raw = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"shell\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"ls\"}}\n\n",
        );
        let resp = parse(raw.as_bytes());
        assert_eq!(resp.stop_reason, "stream_truncated");
        match &resp.blocks[0] {
            Block::ToolUse { input, .. } => assert!(input.get("_raw").is_some()),
            other => panic!("expected tool_use, got {other:?}"),
        }
    }

    #[test]
    fn complete_block_with_malformed_json_becomes_raw_not_run_error() {
        // A model-level JSON slip on an OTHERWISE complete stream (proper
        // content_block_stop, clean end_turn) must not kill the run: the bad
        // input becomes _raw so the tool layer can report it back to the
        // model (same contract as the OpenAI provider).
        let raw = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"shell\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{not json\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let resp = parse(raw.as_bytes());
        assert_eq!(resp.stop_reason, "tool_use");
        match &resp.blocks[0] {
            Block::ToolUse { input, .. } => assert!(input.get("_raw").is_some()),
            other => panic!("expected tool_use, got {other:?}"),
        }
    }

    /// A reader that hands out one batch of bytes, then FAILS every read —
    /// simulating a gateway that holds the connection open after message_stop
    /// and later breaks, with no clean EOF in between. The sync test source
    /// has no idle-timeout notion; the error stands in for "the loop kept
    /// reading past message_stop", which is exactly what the fix removes.
    struct HoldThenErr {
        data: Vec<u8>,
        pos: usize,
    }
    impl std::io::Read for HoldThenErr {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.data.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "gateway broke",
                ));
            }
            let n = (self.data.len() - self.pos).min(b.len());
            b[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn message_stop_ends_the_stream_without_eof() {
        // A1: a tool_use turn whose stream ends with message_stop while the
        // server keeps the body open. Pre-fix the loop read on → the error
        // surfaced as stop_reason "stream_error"; post-fix it breaks at
        // message_stop and returns the REAL stop_reason with blocks intact
        // (the agent's aborted branch would otherwise strip the tool_use and
        // fail the whole run).
        let raw = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"shell\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"ls\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let resp = Anthropic::parse_stream(
            HoldThenErr {
                data: raw.as_bytes().to_vec(),
                pos: 0,
            },
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(resp.stop_reason, "tool_use");
        match &resp.blocks[0] {
            Block::ToolUse { id, input, .. } => {
                assert_eq!(id, "t1");
                assert_eq!(input["command"], "ls");
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
    }

    #[test]
    fn content_block_start_with_inline_input_is_seeded() {
        // B5: a gateway may inline the tool arguments in content_block_start
        // (the official API always sends {} and streams via input_json_delta).
        // With no deltas following, the seeded object must become the input —
        // previously it was dropped entirely (empty json → {}).
        let raw = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t2\",\"name\":\"shell\",\"input\":{\"command\":\"pwd\"}}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let resp = parse(raw.as_bytes());
        assert_eq!(resp.stop_reason, "tool_use");
        match &resp.blocks[0] {
            Block::ToolUse { id, input, .. } => {
                assert_eq!(id, "t2");
                assert_eq!(input["command"], "pwd");
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_serialization_avoids_empty_text_blocks() {
        // B6: Anthropic rejects empty text blocks with 400. An image-bearing
        // result with empty text must emit only the image block; a result with
        // neither text nor image must OMIT `content` (legal per the API)
        // instead of sending `"text": ""`.
        let mk = |content: &str, image: Option<Block>| {
            Message::user(vec![Block::ToolResult {
                tool_use_id: "t1".into(),
                content: content.into(),
                image: image.map(Box::new),
                is_error: false,
            }])
        };
        let img = || Block::Image {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        };

        // image + empty text → [image] only
        let v = Anthropic::message_json(&mk("", Some(img())));
        let blocks = v["content"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1, "got: {blocks:?}");
        assert_eq!(blocks[0]["type"], "image");

        // no image + empty text → content key omitted entirely
        let v = Anthropic::message_json(&mk("", None));
        assert!(
            v["content"][0].get("content").is_none(),
            "content must be omitted, got: {}",
            v["content"][0]
        );

        // image + text → [image, text] (unchanged shape)
        let v = Anthropic::message_json(&mk("ok", Some(img())));
        let blocks = v["content"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "ok");

        // text only → bare string (unchanged shape)
        let v = Anthropic::message_json(&mk("plain", None));
        assert_eq!(v["content"][0]["content"], "plain");
    }

    #[test]
    fn document_block_serializes_as_anthropic_document_source() {
        // PDF support wire shape: a `document` content block with a base64
        // source object, mirroring the image block's structure.
        let m = Message::user(vec![
            Block::Document {
                media_type: "application/pdf".into(),
                data: "JVBERi0=".into(), // b64("%PDF-")
            },
            Block::Text("summarize this".into()),
        ]);
        let v = Anthropic::message_json(&m);
        assert_eq!(v["content"][0]["type"], "document");
        assert_eq!(v["content"][0]["source"]["type"], "base64");
        assert_eq!(v["content"][0]["source"]["media_type"], "application/pdf");
        assert_eq!(v["content"][0]["source"]["data"], "JVBERi0=");
        assert_eq!(v["content"][1]["type"], "text");
    }

    /// A representative agent-turn request: system prompt + two tools + two
    /// messages (so the last-message and last-tool paths are both exercised).
    fn sample_req(cache: bool, effort: Option<&'static str>) -> ChatRequest<'static> {
        let tools = vec![
            ToolDef {
                name: "shell".into(),
                description: "run a shell command".into(),
                schema: json!({"type": "object"}),
            },
            ToolDef {
                name: "read_file".into(),
                description: "read a file".into(),
                schema: json!({"type": "object"}),
            },
        ];
        // Leak so the slice is 'static (test-only; the process exits with it).
        let tools: &'static [ToolDef] = Box::leak(tools.into_boxed_slice());
        let msgs = vec![
            Message::user(vec![Block::Text("hello".into())]),
            Message::assistant(vec![Block::Text("hi there".into())]),
        ];
        let msgs: &'static [Message] = Box::leak(msgs.into_boxed_slice());
        ChatRequest {
            system: Some("you are a helpful agent"),
            messages: msgs,
            tools,
            model: "claude-haiku-4-5-20251001",
            max_tokens: 1024,
            per_request_timeout: None,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cache,
            reasoning_effort: effort,
            temperature: None,
        }
    }

    #[test]
    fn build_body_marks_three_cache_breakpoints_when_cache_on() {
        // cache=true (an agent turn) → up to three ephemeral breakpoints:
        // system array, the last tool def, and the last block of the last
        // message. The whole run stays under Anthropic's 4-breakpoint cap.
        let body = Anthropic::build_body(&sample_req(true, None));
        let s = serde_json::to_string(&body).unwrap();
        let n = s.matches("cache_control").count();
        assert_eq!(n, 3, "expected 3 breakpoints, body was:\n{s}");
        // System becomes a content-block array carrying cache_control, not a
        // bare string.
        assert_eq!(body["system"][0]["type"], "text");
    }

    #[test]
    fn build_body_has_no_cache_control_when_cache_off() {
        // cache=false (a one-off compression-summary request) → no breakpoint
        // markup at all: paying the 1.25× write premium on content that's never
        // re-read would be pure waste. System falls back to a bare string.
        let body = Anthropic::build_body(&sample_req(false, None));
        let s = serde_json::to_string(&body).unwrap();
        assert!(
            !s.contains("cache_control"),
            "one-off request must not carry cache_control, body was:\n{s}"
        );
        assert_eq!(
            body["system"], "you are a helpful agent",
            "bare string form"
        );
    }

    #[test]
    fn build_body_adaptive_thinking_maps_effort() {
        // effort (non-"none") → thinking:{"type":"adaptive"} + top-level effort;
        // "none" → thinking:{"type":"disabled"}; None → no thinking field at all
        // (server default). Forwarded verbatim — no enum mapping.
        let hi = Anthropic::build_body(&sample_req(true, Some("high")));
        assert_eq!(hi["thinking"]["type"], "adaptive");
        assert_eq!(hi["output_config"]["effort"], "high");
        // effort must NOT be a bare top-level field (silently ignored) nor in thinking.
        assert!(hi.get("effort").is_none());
        assert!(hi["thinking"].get("effort").is_none());

        let off = Anthropic::build_body(&sample_req(true, Some("none")));
        assert_eq!(off["thinking"]["type"], "disabled");
        assert!(off.get("effort").is_none());

        let dflt = Anthropic::build_body(&sample_req(true, None));
        assert!(dflt.get("thinking").is_none());
        assert!(dflt.get("effort").is_none());
    }
}
