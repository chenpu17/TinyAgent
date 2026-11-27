#[cfg(test)]
use super::SseReader;
use super::{
    Block, CancellableSse, ChatEvent, ChatRequest, ChatResponse, Message, Provider, Role,
    SseSource, StreamAbort, ToolDef, Usage,
};
use anyhow::{Context, Result};
use serde_json::{json, Value};

/// The Anthropic Messages API provider. Construct via
/// [`Anthropic::new`] or, normally, through
/// [`crate::provider::build`] / [`crate::build_agent`].
pub struct Anthropic {
    client: reqwest::blocking::Client,
    base_url: String,
    api_key: String,
    idle: std::time::Duration,
    /// `Config.extra_body` / `Config.extra_headers`, applied to every
    /// request (see the `with_extra_*` builders).
    extras: super::RequestExtras,
}

impl Anthropic {
    /// `idle` is the maximum gap between SSE chunks before the stream is
    /// treated as stalled (from `limits.stream_idle_secs`, clamped ≥1s by
    /// `provider::build`). The client's own timeout stays the hard backstop.
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
            extras: super::RequestExtras::default(),
        }
    }

    /// Extra top-level request-body fields for every request
    /// (`Config.extra_body`, wired by `provider::build`). Replaces any set
    /// earlier. Ignored unless it is a JSON object; a field the kernel
    /// already writes (`model`, `messages`, …) is never overridden.
    pub fn with_extra_body(mut self, extra_body: Value) -> Self {
        {
            self.extras.set_body(extra_body);
            self
        }
    }

    /// Extra request headers for every request (`Config.extra_headers`,
    /// wired by `provider::build`). Replaces any set earlier. Dropped here:
    /// a header the kernel manages (authentication, content type, protocol
    /// version) and anything that is not a valid header — the config
    /// validation already rejects both with the entry blamed, this is the
    /// guard for a provider built by hand.
    pub fn with_extra_headers(mut self, extra_headers: crate::config::ExtraHeaders) -> Self {
        {
            self.extras.set_headers(&extra_headers);
            self
        }
    }

    /// Both extras as parsed from a config — `provider::build`'s entry.
    pub(crate) fn with_request_extras(mut self, extras: super::RequestExtras) -> Self {
        {
            self.extras = extras;
            self
        }
    }

    /// Map one message to the wire shape. `echo_reasoning` false (thinking
    /// disabled for this request, or the default when called directly) sends
    /// the blocks alone; true splices the message's carried reasoning blocks
    /// back where they appeared — see [`Anthropic::content_with_reasoning`].
    fn message_json(m: &Message, echo_reasoning: bool) -> Value {
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
        let content = if echo_reasoning && m.role == Role::Assistant {
            Self::content_with_reasoning(m, content)
        } else {
            content
        };
        json!({"role": role, "content": content})
    }

    /// Splice a message's carried reasoning (`Message::reasoning`) into its
    /// wire content, each block at the position it had when the model emitted
    /// it (`ReasoningBlock::before`). Reasoning carry, Anthropic side
    /// (`Config.preserved_reasoning`): the Messages API wants a thinking
    /// block sent back verbatim, signature included, for the model to keep
    /// its reasoning across a tool-use loop.
    ///
    /// - Only this family's state (`Message::reasoning_for`): a session
    ///   written by the other provider carries nothing we know how to send.
    /// - A block the state can't rebuild faithfully is skipped, not guessed:
    ///   a thinking block needs its `signature` (the API rejects an unsigned
    ///   one — an unsigned capture was already dropped at parse time, this
    ///   guards hand-edited state), a redacted block its `data`.
    /// - `before` is clamped to the current content: the agent may have
    ///   removed blocks from the message after capture (a truncated
    ///   tool_use), and positions then shift down with them.
    /// - Order is preserved: reasoning blocks that shared a position keep
    ///   emission order and precede the content block that came after.
    fn content_with_reasoning(m: &Message, content: Vec<Value>) -> Vec<Value> {
        let Some(r) = m.reasoning_for(super::REASONING_ANTHROPIC) else {
            return content;
        };
        let blocks: Vec<(usize, Value)> = r
            .blocks
            .iter()
            .filter_map(|b| {
                let v = match b.opaque.get("signature") {
                    Some(sig) if sig.is_string() => json!({
                        "type": "thinking", "thinking": b.text, "signature": sig
                    }),
                    _ => match b.opaque.get("data") {
                        Some(data) if data.is_string() => json!({
                            "type": "redacted_thinking", "data": data
                        }),
                        _ => return None,
                    },
                };
                Some((b.before.min(content.len()), v))
            })
            .collect();
        if blocks.is_empty() {
            return content;
        }
        let mut merged = Vec::with_capacity(content.len().saturating_add(blocks.len()));
        let mut rest = blocks.into_iter().peekable();
        for (i, c) in content.into_iter().enumerate() {
            while rest.peek().is_some_and(|(at, _)| *at <= i) {
                merged.push(rest.next().map(|(_, v)| v).unwrap_or(Value::Null));
            }
            merged.push(c);
        }
        merged.extend(rest.map(|(_, v)| v));
        merged
    }

    /// Whether this request wants reasoning echoed: anything but an explicit
    /// "disable thinking" (`reasoning_effort: "none"` → `thinking: {type:
    /// disabled}`). Thinking blocks belong to requests that have thinking
    /// on — a session that switched effort mid-conversation sends the blocks
    /// alone from then on.
    fn echo_reasoning(req: &ChatRequest) -> bool {
        req.reasoning_effort != Some("none")
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
    /// A `thinking` / `redacted_thinking` content block. Its text still goes
    /// out only as ThinkingDelta events — it never becomes a history block —
    /// but the buffer is no longer discard-only: reasoning carry
    /// (`Config.preserved_reasoning`) needs the block's replayable parts
    /// (see [`ThinkingBuf`]), which finalize files into the response's
    /// `reasoning` side field instead of `blocks`.
    Thinking(ThinkingBuf),
}

/// The replayable state of one streaming thinking block. What the Messages
/// API wants back verbatim is exactly this: the text plus its signature
/// (`thinking`), or the opaque payload (`redacted_thinking`, which carries
/// no readable text at all).
struct ThinkingBuf {
    /// `thinking_delta` text. Empty for a redacted block.
    text: String,
    /// `signature_delta` fragments, concatenated. The signature attests to
    /// the text; the API checks it on the way back in, and an unsigned block
    /// cannot be replayed — one that never got a signature (stream cut
    /// inside the block, a gateway that drops the deltas) is dropped.
    signature: String,
    /// `redacted_thinking`'s `data`, handed over whole at content_block_start.
    redacted: Option<String>,
}

impl ThinkingBuf {
    fn thinking() -> Self {
        Self {
            text: String::new(),
            signature: String::new(),
            redacted: None,
        }
    }

    fn redacted(data: &Value) -> Self {
        Self {
            text: String::new(),
            signature: String::new(),
            redacted: data.as_str().map(str::to_string),
        }
    }

    /// The replayable form of this block, or `None` when there is none (an
    /// unsigned thinking block; a redacted block without data).
    fn into_reasoning_block(self, before: usize) -> Option<super::ReasoningBlock> {
        if let Some(data) = self.redacted {
            return Some(super::ReasoningBlock {
                before,
                text: String::new(),
                opaque: json!({ "data": data }),
            });
        }
        (!self.signature.is_empty()).then(|| super::ReasoningBlock {
            before,
            text: self.text,
            opaque: json!({ "signature": self.signature }),
        })
    }
}

/// Finalize a pending block into a concrete Block, tolerant of incomplete
/// JSON. Shared by the content_block_stop handler and the truncation-recovery
/// flushes (content_block_start overwrite + end-of-stream). Half-delivered
/// tool arguments become {"_raw": ...} instead of failing: a truncated stream
/// surfaces as a stream_truncated stop_reason, and a model's malformed JSON
/// on a COMPLETE stream reaches the tool layer (which reports the bad input
/// back to the model) instead of killing the whole run.
///
/// A thinking block yields no Block — its deltas already went out as
/// ThinkingDelta and it never enters history — but its replayable state, if
/// any, is filed into `reasoning` at `before`, the number of blocks finalized
/// ahead of it (the caller's `blocks.len()`), so an echo can put it back.
fn finalize_pending_lenient(
    p: Pending,
    before: usize,
    reasoning: &mut Vec<super::ReasoningBlock>,
) -> Option<Block> {
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
        // Thinking stays out of `blocks` for good (display-only channel);
        // only its opaque replay state is kept, on the side.
        Pending::Thinking(buf) => {
            reasoning.extend(buf.into_reasoning_block(before));
            None
        }
    }
}

impl Provider for Anthropic {
    fn chat(&self, req: &ChatRequest, on_event: &mut dyn FnMut(ChatEvent)) -> Result<ChatResponse> {
        // Cancel checkpoint before the first POST: the agent's loop-head
        // check runs before the turn starts, but a cancel landing during the
        // previous turn's tool phase would otherwise buy one full request
        // round-trip (connect + upload + TTFB) before CancellableSse's
        // in-stream check notices. Same guard shape and message style as the
        // openai side's 400-resend checkpoint; the run-level retry ladder
        // treats the resulting Err + set flag as EXIT_CANCELLED.
        if req.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("request cancelled before sending to the Anthropic API");
        }
        let mut body = Self::build_body(req);
        // Config extras last, and never over a kernel-written field.
        self.extras.merge_body(&mut body);
        let rb = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json");
        let mut rb = self.extras.apply_headers(rb);
        if let Some(t) = req.per_request_timeout {
            rb = rb.timeout(t);
        }
        let mut resp = rb
            .json(&body)
            .send()
            .context("request to Anthropic API failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            // Capped (mcp.rs MCP_ERR_BODY_MAX twin): the error body's size is
            // the gateway's choice and only feeds the diagnostic — never
            // buffered whole.
            let text = super::capped_err_body(&mut resp);
            // Structured status + historical display text (see ApiStatusError).
            let mut msg = format!("Anthropic API error {status}: {text}");
            if status == reqwest::StatusCode::BAD_REQUEST {
                msg.push_str(Self::missing_reasoning_hint(req, &text));
            }
            return Err(super::ApiStatusError::new(status.as_u16(), msg).into());
        }

        let mut sse = CancellableSse::new(resp, req.cancel.clone(), self.idle)?;
        Self::parse_loop(&mut sse, on_event)
    }
}

impl Anthropic {
    /// The hint for the one 400 about reasoning no request edit can repair
    /// here: with thinking enabled, the Messages API requires the last
    /// assistant message of a tool loop to start with its (signed) thinking
    /// block. Fires only when the 400 reads like that requirement AND this
    /// request carried no thinking block at all — i.e. the carry switch was
    /// off. A request that did echo its blocks and still got the 400 has a
    /// different problem (the blocks are all the API gave), so it gets the
    /// plain text instead of a misleading pointer.
    fn missing_reasoning_hint(req: &ChatRequest, text: &str) -> &'static str {
        let names_block = text.contains("thinking")
            && (text.contains("tool_use") || text.contains("redacted_thinking"));
        if !names_block {
            return "";
        }
        let echoed = Self::echo_reasoning(req)
            && req
                .messages
                .iter()
                .any(|m| m.reasoning_for(super::REASONING_ANTHROPIC).is_some());
        if echoed {
            ""
        } else {
            super::REASONING_CARRY_HINT
        }
    }
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
            "messages": req
                .messages
                .iter()
                .map(|m| Self::message_json(m, Self::echo_reasoning(req)))
                .collect::<Vec<_>>(),
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
        // Reasoning carry: the replayable state of every thinking block
        // finalized in this response, in emission order. The agent decides
        // whether to keep it (`Config.preserved_reasoning`).
        let mut reasoning: Vec<super::ReasoningBlock> = Vec::new();

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
                        reasoning: super::into_reasoning(super::REASONING_ANTHROPIC, reasoning),
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
                        if let Some(b) = finalize_pending_lenient(p, blocks.len(), &mut reasoning) {
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
                        // ThinkingDelta) and its integrity tag via
                        // signature_delta; `redacted_thinking` carries no
                        // readable text, only an opaque `data` payload given
                        // whole here. Neither ever enters `blocks` — but both
                        // are kept, in [`ThinkingBuf`], as the replayable
                        // state the agent may echo back.
                        "thinking" => Pending::Thinking(ThinkingBuf::thinking()),
                        "redacted_thinking" => {
                            Pending::Thinking(ThinkingBuf::redacted(&cb["data"]))
                        }
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
                        // ThinkingDelta (event-only); the buffer also keeps the
                        // text for the replayable state.
                        // `signature_delta` carries the block's integrity tag
                        // — display-silent, but the one thing an echoed
                        // thinking block cannot travel without.
                        "thinking_delta" => {
                            if let Some(Pending::Thinking(buf)) = &mut pending {
                                let t = delta["thinking"].as_str().unwrap_or_default();
                                if !t.is_empty() {
                                    buf.text.push_str(t);
                                    on_event(ChatEvent::ThinkingDelta(t.to_string()));
                                }
                            }
                        }
                        "signature_delta" => {
                            if let Some(Pending::Thinking(buf)) = &mut pending {
                                if let Some(sig) = delta["signature"].as_str() {
                                    buf.signature.push_str(sig);
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
                        if let Some(b) = finalize_pending_lenient(p, blocks.len(), &mut reasoning) {
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
                            // Cache tokens are billed input: a delta carrying
                            // ONLY a cache axis must still flip saw_usage, or
                            // the billed tokens surface as usage: None (the
                            // honest-cost hole the input arm above closed).
                            saw_usage = saw_usage || v > 0;
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
                        reasoning: super::into_reasoning(super::REASONING_ANTHROPIC, reasoning),
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
            if let Some(b) = finalize_pending_lenient(p, blocks.len(), &mut reasoning) {
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
            reasoning: super::into_reasoning(super::REASONING_ANTHROPIC, reasoning),
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
    /// The cache fold has the same duty as the input arm: a gateway that
    /// reports ONLY a cache axis in message_delta (no message_start usage at
    /// all) has billed those tokens — the response must still carry
    /// `usage: Some`, never None.
    #[test]
    fn usage_cache_only_in_message_delta_is_counted() {
        let raw = concat!(
            "data: {\"type\":\"message_start\",\"message\":{}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"cache_read_input_tokens\":900}}\n\n",
        );
        let resp = parse(raw.as_bytes());
        assert_eq!(resp.stop_reason, "end_turn");
        let u = resp
            .usage
            .expect("cache-only delta usage must surface as Some, not None");
        assert_eq!(u.cache_read_input_tokens, 900);
        assert_eq!((u.input_tokens, u.output_tokens), (0, 0));
    }

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
        // enters the assembled `blocks` (it travels in the `reasoning` side
        // field instead, and only when it is replayable — see the
        // `signed_thinking_*` / `redacted_thinking_*` tests below).
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
        // No signature_delta in this stream → nothing replayable to keep
        // (an unsigned thinking block cannot be echoed).
        assert!(resp.reasoning.is_none());
    }

    /// Build a thinking-block SSE stream from (delta-text, delta-signature)
    /// fragments, followed by one text block. `redacted` swaps the block for
    /// a `redacted_thinking` one carrying `data`.
    fn thinking_stream(parts: &[(&str, &str)], redacted: Option<&str>) -> String {
        let mut raw = String::from(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
        );
        if let Some(data) = redacted {
            raw.push_str(&format!(
                "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"redacted_thinking\",\"data\":\"{data}\"}}}}\n\n"
            ));
        } else {
            raw.push_str(
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n\n",
            );
        }
        for (text, sig) in parts {
            if !text.is_empty() {
                raw.push_str(&format!(
                    "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"thinking_delta\",\"thinking\":\"{text}\"}}}}\n\n"
                ));
            }
            if !sig.is_empty() {
                raw.push_str(&format!(
                    "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"signature_delta\",\"signature\":\"{sig}\"}}}}\n\n"
                ));
            }
        }
        raw.push_str("data: {\"type\":\"content_block_stop\",\"index\":0}\n\n");
        raw.push_str(
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        );
        raw.push_str(
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
        );
        raw.push_str("data: {\"type\":\"content_block_stop\",\"index\":1}\n\n");
        raw.push_str(
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        );
        raw
    }

    #[test]
    fn signed_thinking_is_kept_as_replayable_state() {
        // thinking → thinking_delta×2 → signature_delta: the text still
        // streams as ThinkingDelta and stays out of `blocks`, but now the
        // block is also captured — text + joined signature — in the
        // response's `reasoning` side field, positioned ahead of the text
        // block it preceded (`before: 0`).
        let raw = thinking_stream(&[("let me ", "sig-A=="), ("think", "sig-B==")], None);
        let mut thinking = String::new();
        let resp = Anthropic::parse_stream(raw.as_bytes(), &mut |e| {
            if let crate::provider::ChatEvent::ThinkingDelta(t) = e {
                thinking.push_str(&t);
            }
        })
        .unwrap();
        assert_eq!(thinking, "let me think");
        assert_eq!(resp.blocks.len(), 1);
        let r = resp.reasoning.expect("signed thinking must be captured");
        assert_eq!(r.provider, super::super::REASONING_ANTHROPIC);
        assert_eq!(r.blocks.len(), 1);
        assert_eq!(r.blocks[0].before, 0);
        assert_eq!(r.blocks[0].text, "let me think");
        assert_eq!(r.blocks[0].opaque, json!({"signature": "sig-A==sig-B=="}));
    }

    #[test]
    fn unsigned_thinking_is_not_kept() {
        // Text but no signature (a gateway that drops signature_delta, or a
        // stream cut inside the block): nothing an echo could send — the API
        // checks the signature — so the block is not captured at all, while
        // the display feed and the text block are exactly as before.
        let raw = thinking_stream(&[("bare thought", "")], None);
        let mut thinking = String::new();
        let resp = Anthropic::parse_stream(raw.as_bytes(), &mut |e| {
            if let crate::provider::ChatEvent::ThinkingDelta(t) = e {
                thinking.push_str(&t);
            }
        })
        .unwrap();
        assert_eq!(thinking, "bare thought");
        assert!(resp.reasoning.is_none());
        assert_eq!(resp.blocks.len(), 1);
    }

    #[test]
    fn redacted_thinking_is_kept_with_its_payload() {
        // `redacted_thinking` has no readable text — only the opaque `data`
        // given whole at content_block_start. Nothing to display, but the
        // payload is exactly what must travel back.
        let raw = thinking_stream(&[], Some("opaque-blob=="));
        let mut events = 0;
        let resp = Anthropic::parse_stream(raw.as_bytes(), &mut |_| events += 1).unwrap();
        assert_eq!(events, 1, "only the text delta fires an event");
        let r = resp.reasoning.expect("redacted thinking must be captured");
        assert_eq!(r.blocks.len(), 1);
        assert_eq!(r.blocks[0].before, 0);
        assert!(r.blocks[0].text.is_empty());
        assert_eq!(r.blocks[0].opaque, json!({"data": "opaque-blob=="}));
        assert_eq!(resp.blocks.len(), 1);
    }

    #[test]
    fn interleaved_thinking_keeps_its_positions() {
        // thinking, text, thinking: the second thinking block trails one
        // block, so its `before` is 1 — an echo replays each block where it
        // appeared (interleaved thinking), not "all thinking first".
        let raw = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"plan\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"s1\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"thinking\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"more\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"s2\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        );
        let resp = parse(raw.as_bytes());
        assert_eq!(resp.blocks.len(), 1);
        let r = resp.reasoning.unwrap();
        let positions: Vec<(usize, &str)> = r
            .blocks
            .iter()
            .map(|b| (b.before, b.text.as_str()))
            .collect();
        assert_eq!(positions, vec![(0, "plan"), (1, "more")]);
    }

    #[test]
    fn empty_signed_thinking_between_text_and_tool_use_is_echoed_in_place() {
        // A shape a real Messages-API endpoint sent in nearly every tool-call
        // turn (2026-11-26, captured on the wire): a thinking block with no
        // `thinking_delta` at all — empty text and one `signature_delta` —
        // sitting after the text block and ahead of the `tool_use`. Empty text
        // is not "unsigned": the block is kept, and the echo puts it back at
        // index 1 with the signature byte for byte.
        let raw = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Writing it now.\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-empty==\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"shell\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"ls\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":12}}\n\n",
        );
        let mut thinking = String::new();
        let resp = Anthropic::parse_stream(raw.as_bytes(), &mut |e| {
            if let crate::provider::ChatEvent::ThinkingDelta(t) = e {
                thinking.push_str(&t);
            }
        })
        .unwrap();
        assert_eq!(
            thinking, "",
            "no thinking_delta: nothing on the display feed"
        );
        assert_eq!(
            resp.blocks.len(),
            2,
            "text + tool_use; thinking never becomes a block"
        );
        let reasoning = resp
            .reasoning
            .expect("an empty thinking block with a signature is replayable state");
        let positions: Vec<(usize, &str)> = reasoning
            .blocks
            .iter()
            .map(|b| (b.before, b.text.as_str()))
            .collect();
        assert_eq!(positions, vec![(1, "")]);

        // Echoed exactly where it sat: text, thinking, tool_use.
        let mut assistant = Message::assistant(resp.blocks);
        assistant.reasoning = Some(reasoning);
        let content = Anthropic::message_json(&assistant, true)["content"]
            .as_array()
            .cloned()
            .expect("content is a list");
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(
            content[1],
            json!({"type": "thinking", "thinking": "", "signature": "sig-empty=="})
        );
        assert_eq!(content[2]["type"], "tool_use");
        assert_eq!(content[2]["input"], json!({"command": "ls"}));
    }

    #[test]
    fn stream_cut_inside_thinking_keeps_only_completed_blocks() {
        // First thinking block signed and complete; second cut off by a
        // stream error before any signature. The complete one survives in
        // the honest-cost partial, the cut one does not.
        let raw = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"done\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"s1\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"thinking\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"half\"}}\n\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n",
        );
        let resp = parse(raw.as_bytes());
        assert_eq!(resp.stop_reason, "stream_error");
        let r = resp.reasoning.unwrap();
        assert_eq!(r.blocks.len(), 1);
        assert_eq!(r.blocks[0].text, "done");
    }

    /// A request whose assistant message carries reasoning state.
    fn carry_req(effort: Option<&'static str>, provider: &'static str) -> ChatRequest<'static> {
        let tools: &'static [ToolDef] = Box::leak(
            vec![ToolDef {
                name: "shell".into(),
                description: "run a shell command".into(),
                schema: json!({"type": "object"}),
            }]
            .into_boxed_slice(),
        );
        let mut assistant = Message::assistant(vec![Block::Text("working on it".into())]);
        assistant.reasoning = Some(super::super::Reasoning {
            provider: provider.into(),
            blocks: vec![super::super::ReasoningBlock {
                before: 0,
                text: "first thought".into(),
                opaque: json!({"signature": "sig=="}),
            }],
        });
        let msgs: &'static [Message] = Box::leak(
            vec![Message::user(vec![Block::Text("hello".into())]), assistant].into_boxed_slice(),
        );
        ChatRequest {
            system: Some("you are a helpful agent"),
            messages: msgs,
            tools,
            model: "claude-opus-5-5",
            max_tokens: 1024,
            per_request_timeout: None,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cache: false,
            reasoning_effort: effort,
            temperature: None,
        }
    }

    #[test]
    fn build_body_echoes_thinking_ahead_of_the_blocks() {
        // Thinking on: the carried block travels back first in the assistant
        // content, text and signature verbatim.
        let body = Anthropic::build_body(&carry_req(Some("low"), "anthropic"));
        let content = &body["messages"][1]["content"];
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "first thought");
        assert_eq!(content[0]["signature"], "sig==");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "working on it");
        assert_eq!(content.as_array().unwrap().len(), 2);
    }

    #[test]
    fn build_body_echoes_redacted_blocks_too() {
        let req = carry_req(None, "anthropic");
        let redacted = Message {
            reasoning: Some(super::super::Reasoning {
                provider: "anthropic".into(),
                blocks: vec![super::super::ReasoningBlock {
                    before: 0,
                    text: String::new(),
                    opaque: json!({"data": "blob=="}),
                }],
            }),
            ..req.messages[1].clone()
        };
        let v = Anthropic::message_json(&redacted, true);
        assert_eq!(
            v["content"][0],
            json!({"type": "redacted_thinking", "data": "blob=="})
        );
        assert_eq!(v["content"][1]["type"], "text");
    }

    #[test]
    fn build_body_skips_unreplayable_and_foreign_state() {
        // No signature, no data → skipped rather than sent malformed; a
        // foreign provider tag → skipped as well; nothing replayable at all
        // → the wire shape is exactly the pre-carry one.
        let req = carry_req(None, "openai");
        let v = Anthropic::message_json(&req.messages[1], true);
        assert_eq!(v["content"].as_array().unwrap().len(), 1);
        assert_eq!(v["content"][0]["type"], "text");

        let unsigned = Message {
            reasoning: Some(super::super::Reasoning {
                provider: "anthropic".into(),
                blocks: vec![super::super::ReasoningBlock {
                    before: 0,
                    text: "edited".into(),
                    opaque: json!({}),
                }],
            }),
            ..req.messages[1].clone()
        };
        let v = Anthropic::message_json(&unsigned, true);
        assert_eq!(v["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn build_body_omits_reasoning_when_thinking_is_disabled() {
        // `reasoning_effort: "none"` → thinking disabled for this request:
        // the blocks alone travel (a session that switched effort off).
        let off = Anthropic::build_body(&carry_req(Some("none"), "anthropic"));
        assert_eq!(off["thinking"]["type"], "disabled");
        let content = &off["messages"][1]["content"];
        assert_eq!(content.as_array().unwrap().len(), 1);
        assert_eq!(content[0]["type"], "text");
        // Server default (None) is "unknown, probably on" → echoed.
        let dflt = Anthropic::build_body(&carry_req(None, "anthropic"));
        assert_eq!(dflt["messages"][1]["content"][0]["type"], "thinking");
    }

    #[test]
    fn echo_positions_clamp_after_blocks_were_removed() {
        // `before` is relative to the blocks at capture time; the agent may
        // drop a block afterwards (a truncated tool_use), so a now-too-large
        // position clamps to the end instead of panicking or reordering.
        let req = carry_req(None, "anthropic");
        let shifted = Message {
            reasoning: Some(super::super::Reasoning {
                provider: "anthropic".into(),
                blocks: vec![
                    super::super::ReasoningBlock {
                        before: 0,
                        text: "first".into(),
                        opaque: json!({"signature": "a"}),
                    },
                    super::super::ReasoningBlock {
                        before: 7,
                        text: "last".into(),
                        opaque: json!({"signature": "b"}),
                    },
                ],
            }),
            ..req.messages[1].clone()
        };
        let v = Anthropic::message_json(&shifted, true);
        let content = v["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["thinking"], "first");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[2]["thinking"], "last");
    }

    #[test]
    fn missing_thinking_400_names_the_carry_switch() {
        // The Messages API's tool-loop rule: with thinking enabled the last
        // assistant message must start with its thinking block. Carry off →
        // the request carries none → the 400's text gets the switch named.
        let text = "messages.1.content.0.type: Expected `thinking` or `redacted_thinking`, but found `tool_use`";
        let carried = carry_req(Some("high"), "anthropic");
        let bare: &'static [Message] = Box::leak(
            vec![
                carried.messages[0].clone(),
                Message {
                    reasoning: None,
                    ..carried.messages[1].clone()
                },
            ]
            .into_boxed_slice(),
        );
        let off = ChatRequest {
            messages: bare,
            ..carry_req(Some("high"), "anthropic")
        };
        let hint = Anthropic::missing_reasoning_hint(&off, text);
        assert!(hint.contains("`preserved_reasoning: true`"), "{hint}");

        // The request DID echo its blocks and still drew this 400 → a
        // different problem (the blocks are all the API gave): no pointer.
        assert_eq!(Anthropic::missing_reasoning_hint(&carried, text), "");

        // Blocks exist but thinking is disabled for this request, so they are
        // not echoed — from the API's side the trace is missing all the same.
        let disabled = carry_req(Some("none"), "anthropic");
        assert!(Anthropic::missing_reasoning_hint(&disabled, text).contains("preserved_reasoning"));

        // Anything that is not the tool-loop rule keeps its plain text.
        assert_eq!(
            Anthropic::missing_reasoning_hint(&off, "max_tokens: must be greater than 0"),
            ""
        );
        assert_eq!(
            Anthropic::missing_reasoning_hint(&off, "thinking.budget_tokens: invalid"),
            ""
        );
        assert_eq!(Anthropic::missing_reasoning_hint(&off, "overloaded"), "");
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
        let v = Anthropic::message_json(&mk("", Some(img())), false);
        let blocks = v["content"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1, "got: {blocks:?}");
        assert_eq!(blocks[0]["type"], "image");

        // no image + empty text → content key omitted entirely
        let v = Anthropic::message_json(&mk("", None), false);
        assert!(
            v["content"][0].get("content").is_none(),
            "content must be omitted, got: {}",
            v["content"][0]
        );

        // image + text → [image, text] (unchanged shape)
        let v = Anthropic::message_json(&mk("ok", Some(img())), false);
        let blocks = v["content"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "ok");

        // text only → bare string (unchanged shape)
        let v = Anthropic::message_json(&mk("plain", None), false);
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
        let v = Anthropic::message_json(&m, false);
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

    #[test]
    fn oversized_error_body_is_capped_and_marked_truncated() {
        // P2-3 twin of mcp.rs's MCP_ERR_BODY_MAX: the non-2xx error body's
        // size is the (possibly hostile or broken) gateway's choice and only
        // feeds a diagnostic string — an endless "error body" must not be
        // buffered whole. Pinned directly on the shared helper the live path
        // calls (`capped_err_body` → `capped_body_text`) because the
        // Anthropic provider has no loopback HTTP mock harness in its test
        // suite (its tests are pure parse/body tests — standing one up for
        // one read-path assertion would dwarf the change); the openai side
        // pins the same helper end-to-end through its mock
        // (oversized_non_2xx_error_body_is_capped_in_the_error_message).
        use crate::provider::{capped_body_text, ERR_BODY_MAX};
        // Over-cap body → kept to exactly the cap + a truncation marker.
        let big = vec![b'E'; ERR_BODY_MAX + 100];
        let s = capped_body_text(&mut &big[..], ERR_BODY_MAX);
        assert!(s.contains("[truncated at"), "…{}", &s[s.len() - 120..]);
        assert!(
            s.len() <= ERR_BODY_MAX + 100,
            "capped read + marker must stay near the cap, got {}",
            s.len()
        );
        assert!(s.starts_with(&"E".repeat(64)), "kept the body head");
        // Exactly-cap body → kept whole, NO marker (the +1 sentinel byte
        // distinguishes "cut" from "exactly the cap").
        let exact = vec![b'B'; ERR_BODY_MAX];
        let s2 = capped_body_text(&mut &exact[..], ERR_BODY_MAX);
        assert_eq!(s2.len(), ERR_BODY_MAX);
        assert!(!s2.contains("[truncated"), "{s2:.80}…");
        // Small body passes through unchanged.
        assert_eq!(
            capped_body_text(&mut &b"overloaded"[..], ERR_BODY_MAX),
            "overloaded"
        );
    }

    #[test]
    fn chat_aborts_before_the_first_post_when_cancel_is_already_set() {
        // P2 cancel checkpoint (guard at the top of chat): a cancel that
        // landed before this call must not buy the run one more request
        // round-trip. The guard fires BEFORE any connection attempt, so no
        // mock server is needed — point the provider at a port with no
        // listener (a freshly-dropped ephemeral bind). If the guard regresses,
        // the connect failure surfaces instead ("request to Anthropic API
        // failed") and the message assertions below fail. Same-type guard as
        // the openai side's 400-resend checkpoint
        // (fix_resend_aborts_when_cancelled_during_the_400).
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("reqwest client");
        let provider = Anthropic::new(
            client,
            format!("http://127.0.0.1:{port}"),
            "k",
            std::time::Duration::from_secs(30),
        );
        let m = vec![Message::user(vec![Block::Text("one".into())])];
        let req = ChatRequest {
            system: None,
            messages: &m,
            tools: &[],
            model: "claude-haiku-4-5-20251001",
            max_tokens: 100,
            per_request_timeout: Some(std::time::Duration::from_secs(10)),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            cache: true,
            reasoning_effort: None,
            temperature: None,
        };
        let err = match provider.chat(&req, &mut |_| {}) {
            Err(e) => e,
            Ok(_) => panic!("chat must abort before sending when cancel is already set"),
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cancelled"),
            "error must name the cancel: {msg}"
        );
        assert!(
            !msg.contains("request to Anthropic API failed"),
            "the guard must fire before any connection attempt: {msg}"
        );
    }

    /// `Config.extra_body` / `extra_headers` reach the wire through the one
    /// production wiring point (`provider::build`): new fields and headers
    /// ride along on every request, while a kernel-written body field or
    /// header keeps its value — here from a config that skipped the
    /// load-time check (built straight from JSON, reserved entries and all).
    #[test]
    fn chat_sends_the_config_extras_on_the_wire() {
        use crate::provider::testhttp;
        let gw = testhttp::gateway_inspected(|_| testhttp::sse_anthropic("ok"));
        let cfg: crate::config::Config = serde_json::from_str(&format!(
            r#"{{"provider":"anthropic","api_key_env":"K","model":"m","base_url":{:?},
                "extra_body":{{"top_k":5,"model":"smuggled","max_tokens":1,"stream":false}},
                "extra_headers":{{"anthropic-beta":"flag-1","Authorization":"Bearer evil",
                                  "X-Tenant":"tenant-7"}}}}"#,
            gw.url
        ))
        .unwrap();
        let prov = super::super::build(&cfg, "k", reqwest::blocking::Client::new());
        let req = sample_req(false, None);
        let resp = prov.chat(&req, &mut |_| {}).unwrap();
        assert!(
            matches!(resp.blocks.as_slice(), [Block::Text(t)] if t == "ok"),
            "{:?}",
            resp.blocks
        );
        let bodies = gw.bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["top_k"], 5);
        assert_eq!(bodies[0]["model"], req.model);
        assert_eq!(bodies[0]["max_tokens"], req.max_tokens);
        assert_eq!(
            bodies[0]["stream"], true,
            "streaming is the kernel's decision"
        );
        let heads = gw.heads.lock().unwrap();
        let head = &heads[0];
        for wanted in [
            "x-api-key: k",
            "anthropic-version: 2023-06-01",
            "content-type: application/json",
            "anthropic-beta: flag-1",
            "x-tenant: tenant-7",
        ] {
            assert!(
                head.iter().any(|l| l == wanted),
                "{wanted} missing from {head:?}"
            );
        }
        assert!(
            !head.iter().any(|l| l.starts_with("authorization:")),
            "the key header cannot be swapped from the extras: {head:?}"
        );
    }
}
