use super::{Block, CancellableSse, ChatEvent, ChatRequest, ChatResponse, Message, Provider, Role, SseSource, StreamAbort, ToolDef, Usage};
#[cfg(test)]
use super::SseReader;
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
                // Join multiple Text blocks with a newline — a bare collect()
                // mashes adjacent blocks together ("foo"+"bar" → "foobar"),
                // corrupting the transcript the model reads back.
                let text: String = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
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
            "stream_options": {"include_usage": true},
            "messages": messages,
        });
        if !req.tools.is_empty() {
            body["tools"] = Self::tools_json(req.tools);
        }

        let mut resp = self.post(&body, req.per_request_timeout)?;

        // A 400 may name a field this gateway doesn't accept (newer reasoning
        // models reject `max_tokens`; older gateways/vLLM reject the unknown
        // `stream_options`). Apply known fixes and retry — at most a couple of
        // times. Each fix mutates a distinct field, so a gateway rejecting BOTH
        // (reasoning model on an old vLLM gateway) is satisfied by two sequential
        // fixes; when no known fix applies the 400 bails with the text. The cap
        // is a backstop — there are only two known fixes, and a fix that can't
        // change anything returns None and bails, so the loop always converges.
        for _ in 0..3 {
            if resp.status() != reqwest::StatusCode::BAD_REQUEST {
                break;
            }
            let text = resp.text().unwrap_or_default();
            match Self::body_fix_for_400(&text, &body) {
                Some(fixed) => {
                    body = fixed;
                    resp = self
                        .post(&body, req.per_request_timeout)
                        .context("retry after adjusting request for 400 failed")?;
                }
                None => bail!("OpenAI-compatible API error 400: {text}"),
            }
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            bail!("OpenAI-compatible API error {status}: {text}");
        }

        let mut sse = CancellableSse::new(resp, req.cancel.clone());
        Self::parse_loop(&mut sse, on_event)
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

    /// On a 400, return an adjusted request body to retry once with, when the
    /// error names a field this gateway doesn't accept:
    /// - `max_tokens`     → rename to `max_completion_tokens` (reasoning models).
    /// - `stream_options` → drop it (older gateways / vLLM). Usage then goes
    ///   unreported, which `parse_loop` already tolerates as `None`.
    ///
    /// Returns `None` for an unknown 400 (the caller bails with the error text).
    fn body_fix_for_400(text: &str, body: &Value) -> Option<Value> {
        let mut next = body.clone();
        let obj = next.as_object_mut()?;
        if text.contains("max_tokens") {
            if let Some(mt) = obj.remove("max_tokens") {
                obj.insert("max_completion_tokens".into(), mt);
                return Some(next);
            }
        }
        if text.contains("stream_options") && obj.remove("stream_options").is_some() {
            return Some(next);
        }
        None
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

    /// Streaming core shared by the sync test path (`SseReader`) and the live
    /// path (`CancellableSse`). A cancel/idle abort surfaces as a stop_reason
    /// carrying whatever partial text/calls arrived — mirroring stream_truncated,
    /// so the agent ends the run cleanly instead of hanging on a half-open
    /// connection.
    fn parse_loop<S: SseSource>(
        src: &mut S,
        on_event: &mut dyn FnMut(ChatEvent),
    ) -> Result<ChatResponse> {
        let mut text = String::new();
        let mut calls: std::collections::BTreeMap<u64, PendingCall> = Default::default();
        let mut cur_idx: u64 = 0;
        let mut finish_reason = String::from("stop");
        let mut got_finish = false;
        let mut abort: Option<&'static str> = None;
        let mut usage: Option<Usage> = None;

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
                // A mid-stream read failure (connection reset, truncated body)
                // must NOT be treated as a clean end-of-stream — that would
                // assemble a partial response and report it as success. Bail so
                // the request fails and is retried as transient.
                Err(StreamAbort::StreamError(msg)) => {
                    bail!("OpenAI stream read error: {msg}");
                }
            };
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
            // Some gateways serialize `"error": null` on success chunks; treat
            // that as no error (filter out null).
            if let Some(err) = ev.get("error").filter(|v| !v.is_null()) {
                bail!(
                    "OpenAI stream error: {}",
                    err["message"].as_str().unwrap_or("(no message)")
                );
            }
            // Usage may ride on the final chunk whether or not it carries a
            // choice: standard OpenAI sends an empty-choices trailer, but some
            // DeepSeek/vLLM/Ollama gateways attach usage to the LAST chunk that
            // also has finish_reason (a non-null choice). Parse it on every
            // event so neither shape silently drops the cost accounting.
            // (Reassigning on later chunks is harmless — usage only appears
            // once, at the end.)
            if let Some(u) = ev.get("usage").filter(|v| !v.is_null()) {
                // OpenAI's `prompt_tokens` is the TOTAL prompt (cached +
                // uncached); `cached_tokens` is the subset served from
                // cache. Subtract so `input_tokens` stays non-cached-only
                // — matching Anthropic's invariant on the Usage struct and
                // keeping the billed-input formula honest across providers
                // (otherwise cached tokens count full-price here AND again
                // as a cache read). cache_creation stays 0: OpenAI's
                // implicit caching has no separate "creation" event.
                let cached = u["prompt_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                let prompt = u["prompt_tokens"].as_u64().unwrap_or(0);
                usage = Some(Usage {
                    input_tokens: prompt.saturating_sub(cached),
                    output_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: cached,
                });
            }
            let choice = &ev["choices"][0];
            if choice.is_null() {
                // Empty-choices trailer (the standard OpenAI usage chunk); the
                // usage was already captured above, so nothing else to do.
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

        // Normalize to Anthropic-style stop reasons. An explicit abort
        // (cancel/idle) wins; otherwise a missing finish_reason means the
        // stream was truncated, not a clean "stop" — reporting it honestly
        // keeps batch/CI from treating an incomplete response as success.
        let stop_reason = if let Some(r) = abort {
            r.into()
        } else if !got_finish {
            "stream_truncated".into()
        } else {
            match finish_reason.as_str() {
                "stop" => "end_turn".into(),
                "tool_calls" => "tool_use".into(),
                "length" => "max_tokens".into(),
                other => other.into(),
            }
        };
        Ok(ChatResponse { blocks, stop_reason, usage })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    #[test]
    fn body_fix_renames_max_tokens() {
        let body = json!({
            "model": "m", "max_tokens": 100,
            "stream_options": {"include_usage": true},
        });
        let fixed =
            OpenAi::body_fix_for_400("Unrecognized request argument: max_tokens", &body).unwrap();
        // Renamed, old key gone; stream_options untouched on a max_tokens error.
        assert!(fixed.get("max_tokens").is_none());
        assert_eq!(fixed.get("max_completion_tokens"), Some(&json!(100)));
        assert!(fixed.get("stream_options").is_some());
    }

    #[test]
    fn body_fix_drops_stream_options() {
        // Older OpenAI-compatible gateways reject the unknown stream_options
        // field — drop it and retry (usage then goes unreported as None).
        let body = json!({
            "model": "m", "max_tokens": 100,
            "stream_options": {"include_usage": true},
        });
        let fixed = OpenAi::body_fix_for_400("Unknown argument: stream_options", &body).unwrap();
        assert!(fixed.get("stream_options").is_none());
        assert_eq!(fixed.get("max_tokens"), Some(&json!(100))); // untouched
    }

    #[test]
    fn body_fix_unknown_400_returns_none() {
        // A 400 that names neither field has no known fix → caller bails.
        let body = json!({"model": "m", "max_tokens": 100});
        assert!(OpenAi::body_fix_for_400("your account is out of credits", &body).is_none());
    }

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
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":9,\"total_tokens\":51}}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |_| {}).unwrap();
        assert_eq!(resp.stop_reason, "max_tokens");
        assert!(matches!(&resp.blocks[0], Block::Text(t) if t == "partial"));
        assert_eq!(
            resp.usage.map(|u| (u.input_tokens, u.output_tokens)),
            Some((42, 9))
        );
    }

    #[test]
    fn stream_usage_reads_cached_tokens() {
        // OpenAI reports cache hits as prompt_tokens_details.cached_tokens on
        // the final usage chunk. cached_tokens is a subset of prompt_tokens, so
        // input_tokens must be prompt - cached (non-cached only, matching
        // Anthropic); cache_creation stays 0 (OpenAI implicit caching).
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"x\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":3,\"total_tokens\":45,\"prompt_tokens_details\":{\"cached_tokens\":30}}}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |_| {}).unwrap();
        let u = resp.usage.expect("usage frame present");
        assert_eq!(u.input_tokens, 12); // 42 total − 30 cached = 12 non-cached
        assert_eq!(u.output_tokens, 3);
        assert_eq!(u.cache_creation_input_tokens, 0);
        assert_eq!(u.cache_read_input_tokens, 30);
    }

    #[test]
    fn stream_usage_none_when_not_requested() {
        // A server that didn't get include_usage (or never sends the final
        // totals chunk) leaves usage as None — the TUI just won't show tokens.
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |_| {}).unwrap();
        assert!(resp.usage.is_none());
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

    #[test]
    fn stream_usage_attaches_to_chunk_with_finish_reason() {
        // DeepSeek/vLLM/Ollama gateways attach `usage` to the LAST chunk that
        // also carries a non-null choice + finish_reason — NOT the standard
        // OpenAI empty-choices trailer (which stream_usage_reads_cached_tokens
        // covers). Usage must be parsed on every event regardless of whether
        // the chunk carries a choice, or this shape silently drops cost.
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |_| {}).unwrap();
        assert_eq!(
            resp.usage.map(|u| (u.input_tokens, u.output_tokens)),
            Some((10, 2)),
            "usage on a choice-carrying chunk must be captured"
        );
    }

    #[test]
    fn stream_error_null_on_success_chunk_does_not_bail() {
        // Some gateways serialize `"error": null` on success chunks. The null
        // filter must skip it — without it, a healthy stream would bail as an
        // error the moment the first delta carries an explicit null error.
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}],\"error\":null}\n\n",
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}],\"error\":null}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |_| {}).unwrap();
        assert!(matches!(&resp.blocks[0], Block::Text(t) if t == "hi"));
        assert_eq!(resp.stop_reason, "end_turn");
    }
}
