use super::{Block, ChatEvent, ChatRequest, ChatResponse, Message, Provider, Role, SseReader, ToolDef};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

pub struct Anthropic {
    client: reqwest::blocking::Client,
    base_url: String,
    api_key: String,
}

impl Anthropic {
    pub fn new(client: reqwest::blocking::Client, base_url: String, api_key: &str) -> Self {
        Self {
            client,
            base_url,
            api_key: api_key.to_string(),
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
                Block::ToolUse { id, name, input } => json!({
                    "type": "tool_use", "id": id, "name": name, "input": input
                }),
                Block::ToolResult {
                    tool_use_id,
                    content,
                    image,
                    is_error,
                } => {
                    let body = match image.as_deref() {
                        Some(Block::Image { media_type, data }) => json!([
                            {"type": "image",
                             "source": {"type": "base64", "media_type": media_type, "data": data}},
                            {"type": "text", "text": content}
                        ]),
                        _ => json!(content),
                    };
                    json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": body,
                        "is_error": is_error
                    })
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
    ToolUse { id: String, name: String, json: String },
}

/// Finalize a fully-assembled pending block into a concrete Block. Shared by
/// the content_block_stop handler and the truncation-recovery flushes
/// (content_block_start overwrite + end-of-stream).
fn finalize_pending(p: Pending) -> Result<Block> {
    Ok(match p {
        Pending::Text(t) => Block::Text(t),
        Pending::ToolUse { id, name, json } => {
            let input = if json.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&json)
                    .map_err(|e| anyhow!("bad tool input JSON from model: {e}"))?
            };
            Block::ToolUse { id, name, input }
        }
    })
}

impl Provider for Anthropic {
    fn chat(&self, req: &ChatRequest, on_event: &mut dyn FnMut(ChatEvent)) -> Result<ChatResponse> {
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "stream": true,
            "messages": req.messages.iter().map(Self::message_json).collect::<Vec<_>>(),
        });
        if let Some(sys) = req.system {
            body["system"] = json!(sys);
        }
        if !req.tools.is_empty() {
            body["tools"] = Self::tools_json(req.tools);
        }

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
            bail!("Anthropic API error {status}: {text}");
        }

        Self::parse_stream(resp, on_event)
    }
}

impl Anthropic {
    /// Assemble a full response from an SSE byte stream (split out for testing).
    fn parse_stream<R: std::io::Read>(
        reader: R,
        on_event: &mut dyn FnMut(ChatEvent),
    ) -> Result<ChatResponse> {
        let mut sse = SseReader::new(reader);
        let mut blocks: Vec<Block> = Vec::new();
        let mut pending: Option<Pending> = None;
        let mut stop_reason = String::from("end_turn");

        while let Some(data) = sse.next_data() {
            let ev: Value = match serde_json::from_str(&data) {
                Ok(v) => v,
                Err(_) => continue, // ignore unparseable keep-alives
            };
            match ev["type"].as_str().unwrap_or("") {
                "content_block_start" => {
                    // A previous block that never received content_block_stop
                    // (truncated stream) would be overwritten here — flush it.
                    if let Some(p) = pending.take() {
                        blocks.push(finalize_pending(p)?);
                    }
                    let cb = &ev["content_block"];
                    pending = Some(match cb["type"].as_str().unwrap_or("") {
                        "tool_use" => Pending::ToolUse {
                            id: cb["id"].as_str().unwrap_or_default().to_string(),
                            name: cb["name"].as_str().unwrap_or_default().to_string(),
                            json: String::new(),
                        },
                        _ => Pending::Text(String::new()),
                    });
                }
                "content_block_delta" => {
                    let delta = &ev["delta"];
                    match delta["type"].as_str().unwrap_or("") {
                        "text_delta" => {
                            let t = delta["text"].as_str().unwrap_or_default();
                            if let Some(Pending::Text(buf)) = &mut pending {
                                buf.push_str(t);
                            }
                            on_event(ChatEvent::TextDelta(t.to_string()));
                        }
                        "input_json_delta" => {
                            if let Some(Pending::ToolUse { json, .. }) = &mut pending {
                                json.push_str(delta["partial_json"].as_str().unwrap_or_default());
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    if let Some(p) = pending.take() {
                        blocks.push(finalize_pending(p)?);
                    }
                }
                "message_delta" => {
                    if let Some(r) = ev["delta"]["stop_reason"].as_str() {
                        stop_reason = r.to_string();
                    }
                }
                "error" => {
                    bail!(
                        "Anthropic stream error: {}",
                        ev["error"]["message"].as_str().unwrap_or("(no message)")
                    );
                }
                _ => {}
            }
        }

        // A stream cut off mid-block (network drop, or per_request_timeout
        // firing during generation) leaves a pending block unflushed. Flush
        // it rather than silently dropping a tool call or trailing text.
        if let Some(p) = pending.take() {
            blocks.push(finalize_pending(p)?);
        }

        Ok(ChatResponse { blocks, stop_reason })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &[u8]) -> ChatResponse {
        Anthropic::parse_stream(raw, &mut |_| {}).unwrap()
    }

    #[test]
    fn stream_text_and_stop_reason() {
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        );
        let resp = parse(raw.as_bytes());
        assert_eq!(resp.stop_reason, "end_turn");
        assert_eq!(resp.blocks.len(), 1);
        assert!(matches!(&resp.blocks[0], Block::Text(t) if t == "Hello world"));
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
    fn stream_error_event_bails() {
        let raw = "data: {\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}\n\n";
        assert!(Anthropic::parse_stream(raw.as_bytes(), &mut |_| {}).is_err());
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
}
