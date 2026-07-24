use super::{Block, ChatEvent, ChatRequest, ChatResponse, Message, Provider, Role, SseReader, ToolDef};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// OpenAI-compatible provider: works with OpenAI, DeepSeek, vLLM, Ollama, etc.
pub struct OpenAi {
    client: reqwest::blocking::Client,
    base_url: String,
    api_key: String,
}

impl OpenAi {
    pub fn new(client: reqwest::blocking::Client, base_url: String, api_key: &str) -> Self {
        Self {
            client,
            base_url,
            api_key: api_key.to_string(),
        }
    }

    /// One internal Message may expand to several wire messages
    /// (tool results become separate `tool` role messages).
    fn message_jsons(m: &Message) -> Vec<Value> {
        match m.role {
            Role::Assistant => {
                let text: String = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect();
                let calls: Vec<Value> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::ToolUse { id, name, input } => Some(json!({
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": input.to_string()}
                        })),
                        _ => None,
                    })
                    .collect();
                let mut msg = json!({"role": "assistant"});
                if text.is_empty() {
                    msg["content"] = Value::Null;
                } else {
                    msg["content"] = json!(text);
                }
                if !calls.is_empty() {
                    msg["tool_calls"] = json!(calls);
                }
                vec![msg]
            }
            Role::User => {
                let mut out = Vec::new();
                let mut parts: Vec<Value> = Vec::new();
                // OpenAI `tool` messages can't carry images. Collect any
                // tool-result images and emit them as ONE trailing `user`
                // message after every `tool` message, so the wire order stays
                // `assistant(tool_calls) → tool* → user`. Inserting a user
                // message between two tool messages makes strict servers reject
                // the request when a non-last parallel call returned an image.
                let mut trailing_images: Vec<Value> = Vec::new();
                let flush_user = |parts: &mut Vec<Value>, out: &mut Vec<Value>| {
                    if !parts.is_empty() {
                        out.push(json!({"role": "user", "content": parts}));
                        parts.clear();
                    }
                };
                for b in &m.content {
                    match b {
                        Block::Text(t) => parts.push(json!({"type": "text", "text": t})),
                        Block::Image { media_type, data } => parts.push(json!({
                            "type": "image_url",
                            "image_url": {"url": format!("data:{media_type};base64,{data}")}
                        })),
                        Block::ToolResult {
                            tool_use_id,
                            content,
                            image,
                            is_error,
                        } => {
                            // Any pending plain user content must precede tool messages.
                            flush_user(&mut parts, &mut out);
                            let note = if *is_error {
                                format!("[tool error] {content}")
                            } else {
                                content.clone()
                            };
                            out.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": note
                            }));
                            if let Some(Block::Image { media_type, data }) = image.as_deref() {
                                trailing_images.push(json!({
                                    "type": "image_url",
                                    "image_url": {"url": format!("data:{media_type};base64,{data}")}
                                }));
                            }
                        }
                        Block::ToolUse { .. } => {} // not valid in user role; ignore
                    }
                }
                flush_user(&mut parts, &mut out);
                if !trailing_images.is_empty() {
                    let mut content =
                        vec![json!({"type": "text", "text": "[image(s) returned by tool]"})];
                    content.append(&mut trailing_images);
                    out.push(json!({"role": "user", "content": content}));
                }
                out
            }
        }
    }

    fn tools_json(tools: &[ToolDef]) -> Value {
        let list: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.schema
                    }
                })
            })
            .collect();
        json!(list)
    }
}

#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

impl Provider for OpenAi {
    fn chat(&self, req: &ChatRequest, on_event: &mut dyn FnMut(ChatEvent)) -> Result<ChatResponse> {
        let mut messages: Vec<Value> = Vec::new();
        if let Some(sys) = req.system {
            messages.push(json!({"role": "system", "content": sys}));
        }
        for m in req.messages {
            messages.extend(Self::message_jsons(m));
        }

        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "stream": true,
            "messages": messages,
        });
        if !req.tools.is_empty() {
            body["tools"] = Self::tools_json(req.tools);
        }

        let resp = self.post(&body, req.per_request_timeout)?;

        // Newer OpenAI reasoning models (o1/o3/gpt-4.1…) reject max_tokens;
        // retry once with max_completion_tokens when the error names it.
        let resp = if resp.status() == reqwest::StatusCode::BAD_REQUEST {
            let text = resp.text().unwrap_or_default();
            if text.contains("max_tokens") {
                // remove() the old key: take() would leave a
                // "max_tokens": null entry that some strict servers reject
                // again on the retry.
                if let Some(obj) = body.as_object_mut() {
                    if let Some(mt) = obj.remove("max_tokens") {
                        obj.insert("max_completion_tokens".into(), mt);
                    }
                }
                self.post(&body, req.per_request_timeout)
                    .context("retry with max_completion_tokens failed")?
            } else {
                bail!("OpenAI-compatible API error 400: {text}");
            }
        } else {
            resp
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            bail!("OpenAI-compatible API error {status}: {text}");
        }

        Self::parse_stream(resp, on_event)
    }
}

impl OpenAi {
    fn post(
        &self,
        body: &Value,
        timeout: Option<std::time::Duration>,
    ) -> Result<reqwest::blocking::Response> {
        let mut rb = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json");
        if let Some(t) = timeout {
            rb = rb.timeout(t);
        }
        rb.json(body)
            .send()
            .context("request to OpenAI-compatible API failed")
    }

    /// Assemble a full response from an SSE byte stream (split out for testing).
    fn parse_stream<R: std::io::Read>(
        reader: R,
        on_event: &mut dyn FnMut(ChatEvent),
    ) -> Result<ChatResponse> {
        let mut sse = SseReader::new(reader);
        let mut text = String::new();
        let mut calls: std::collections::BTreeMap<u64, PendingCall> = Default::default();
        let mut cur_idx: u64 = 0;
        let mut finish_reason = String::from("stop");
        let mut got_finish = false;

        while let Some(data) = sse.next_data() {
            if data == "[DONE]" {
                break;
            }
            let ev: Value = match serde_json::from_str(&data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Streaming errors arrive as {"error": {...}} with no `choices`.
            // Without this they'd be skipped (choice is null → continue) and a
            // content_filter / overloaded error would masquerade as an empty
            // success — possibly with half-received tool args fed to _raw.
            if let Some(err) = ev.get("error") {
                bail!(
                    "OpenAI stream error: {}",
                    err["message"].as_str().unwrap_or("(no message)")
                );
            }
            let choice = &ev["choices"][0];
            if choice.is_null() {
                continue;
            }
            if let Some(r) = choice["finish_reason"].as_str() {
                finish_reason = r.to_string();
                got_finish = true;
            }
            let delta = &choice["delta"];
            if let Some(t) = delta["content"].as_str() {
                text.push_str(t);
                on_event(ChatEvent::TextDelta(t.to_string()));
            }
            if let Some(tc) = delta["tool_calls"].as_array() {
                for call in tc {
                    // Servers that omit `index`: a chunk carrying an unseen
                    // id starts a new call, id-less chunks continue the last.
                    let idx = match call["index"].as_u64() {
                        Some(i) => {
                            cur_idx = i;
                            i
                        }
                        None => {
                            if let Some(id) = call["id"].as_str() {
                                let seen = calls
                                    .get(&cur_idx)
                                    .is_some_and(|e| !e.id.is_empty() && e.id != id);
                                if seen {
                                    cur_idx += 1;
                                }
                            }
                            cur_idx
                        }
                    };
                    let entry = calls.entry(idx).or_default();
                    if let Some(id) = call["id"].as_str() {
                        entry.id = id.to_string();
                    }
                    if let Some(name) = call["function"]["name"].as_str() {
                        entry.name = name.to_string();
                    }
                    if let Some(args) = call["function"]["arguments"].as_str() {
                        entry.arguments.push_str(args);
                    }
                }
            }
        }

        let mut blocks: Vec<Block> = Vec::new();
        if !text.is_empty() {
            blocks.push(Block::Text(text));
        }
        for (_, c) in calls {
            let input = if c.arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&c.arguments).unwrap_or_else(|_| json!({"_raw": c.arguments}))
            };
            blocks.push(Block::ToolUse {
                id: c.id,
                name: c.name,
                input,
            });
        }

        // Normalize to Anthropic-style stop reasons. If no finish_reason
        // arrived the stream was truncated — say so instead of defaulting to
        // "stop", which would make batch/CI treat an incomplete response as
        // success.
        let stop_reason = if !got_finish {
            "stream_truncated".into()
        } else {
            match finish_reason.as_str() {
                "stop" => "end_turn".into(),
                "tool_calls" => "tool_use".into(),
                "length" => "max_tokens".into(),
                other => other.into(),
            }
        };
        Ok(ChatResponse { blocks, stop_reason })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    #[test]
    fn tool_result_becomes_tool_message() {
        let m = Message::user(vec![Block::ToolResult {
            tool_use_id: "call_1".into(),
            content: "ok".into(),
            image: None,
            is_error: false,
        }]);
        let msgs = OpenAi::message_jsons(&m);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["tool_call_id"], "call_1");
        assert_eq!(msgs[0]["content"], "ok");
    }

    #[test]
    fn image_tool_result_appends_user_image() {
        let m = Message::user(vec![Block::ToolResult {
            tool_use_id: "call_2".into(),
            content: "see image".into(),
            image: Some(Box::new(Block::Image {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            })),
            is_error: false,
        }]);
        let msgs = OpenAi::message_jsons(&m);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"][1]["type"], "image_url");
    }

    #[test]
    fn assistant_tool_calls_serialized() {
        let m = Message::assistant(vec![
            Block::Text("checking".into()),
            Block::ToolUse {
                id: "c1".into(),
                name: "shell".into(),
                input: json!({"command": "ls"}),
            },
        ]);
        let msgs = OpenAi::message_jsons(&m);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"], "checking");
        assert_eq!(msgs[0]["tool_calls"][0]["function"]["name"], "shell");
        assert_eq!(
            msgs[0]["tool_calls"][0]["function"]["arguments"],
            "{\"command\":\"ls\"}"
        );
    }

    #[test]
    fn stream_parallel_tool_calls_split_args() {
        // Two tool calls, arguments delivered in chunks, indexed properly.
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"shell\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"comma\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"c2\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"nd\\\":\\\"ls\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"tool_calls\",\"delta\":{}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |_| {}).unwrap();
        assert_eq!(resp.stop_reason, "tool_use");
        assert_eq!(resp.blocks.len(), 2);
        match &resp.blocks[0] {
            Block::ToolUse { id, name, input } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "shell");
                assert_eq!(input["command"], "ls");
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        match &resp.blocks[1] {
            Block::ToolUse { id, name, input } => {
                assert_eq!(id, "c2");
                assert_eq!(name, "read_file");
                assert_eq!(input["path"], "a.txt");
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
    }

    #[test]
    fn stream_tool_calls_without_index() {
        // Server omits `index`: new id starts a new call, id-less chunks
        // continue the previous one. Arguments must not cross-contaminate.
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"c1\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"a\\\":1}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"c2\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"b\\\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"function\":{\"arguments\":\":2}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"tool_calls\",\"delta\":{}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |_| {}).unwrap();
        assert_eq!(resp.blocks.len(), 2);
        match &resp.blocks[0] {
            Block::ToolUse { id, input, .. } => {
                assert_eq!(id, "c1");
                assert_eq!(input["a"], 1);
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        match &resp.blocks[1] {
            Block::ToolUse { id, input, .. } => {
                assert_eq!(id, "c2");
                assert_eq!(input["b"], 2);
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
    }

    #[test]
    fn stream_text_and_length_reason() {
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"partial\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"length\",\"delta\":{}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |_| {}).unwrap();
        assert_eq!(resp.stop_reason, "max_tokens");
        assert!(matches!(&resp.blocks[0], Block::Text(t) if t == "partial"));
    }

    #[test]
    fn stream_error_event_bails() {
        // A streaming error arrives with no `choices`; without the explicit
        // check it would be skipped and surface as an empty success.
        let raw = "data: {\"error\":{\"message\":\"content filter\",\"type\":\"content_filter\"}}\n\ndata: [DONE]\n\n";
        match OpenAi::parse_stream(raw.as_bytes(), &mut |_| {}) {
            Err(e) => assert!(format!("{e:#}").contains("content filter")),
            Ok(_) => panic!("expected stream error, got success"),
        }
    }

    #[test]
    fn parallel_image_tool_results_keep_tool_order() {
        // Two parallel tool results, BOTH carrying images, must serialize as
        // tool, tool, user(images) — inserting a user message between two tool
        // messages makes strict servers reject the request.
        let m = Message::user(vec![
            Block::ToolResult {
                tool_use_id: "c1".into(),
                content: "r1".into(),
                image: Some(Box::new(Block::Image {
                    media_type: "image/png".into(),
                    data: "A1".into(),
                })),
                is_error: false,
            },
            Block::ToolResult {
                tool_use_id: "c2".into(),
                content: "r2".into(),
                image: Some(Box::new(Block::Image {
                    media_type: "image/png".into(),
                    data: "A2".into(),
                })),
                is_error: false,
            },
        ]);
        let msgs = OpenAi::message_jsons(&m);
        let roles: Vec<&str> = msgs
            .iter()
            .map(|v| v["role"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(roles, vec!["tool", "tool", "user"]);
        // The trailing user message carries both images.
        let img_count = msgs[2]["content"]
            .as_array()
            .map(|c| {
                c.iter()
                    .filter(|p| p["type"] == "image_url")
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(img_count, 2);
    }
}
