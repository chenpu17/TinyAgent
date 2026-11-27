#[cfg(test)]
use super::SseReader;
use super::{
    Block, CancellableSse, ChatEvent, ChatRequest, ChatResponse, Message, Provider, Role,
    SseSource, StreamAbort, ToolDef, Usage,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// OpenAI-compatible provider: works with OpenAI, DeepSeek, vLLM, Ollama, etc.
pub struct OpenAi {
    client: reqwest::blocking::Client,
    base_url: String,
    api_key: String,
    idle: std::time::Duration,
    /// Body fixes learned from prior 400s on THIS gateway, applied proactively
    /// to every subsequent request so a reasoning model on an old gateway
    /// doesn't re-trigger the same 400 (and its wasted round-trip) on every
    /// turn. Mutex because `chat` takes `&self` and the agent reuses one
    /// provider across turns; it never contends in practice (the agent's chat
    /// loop is single-threaded), but interior mutability from a shared ref is
    /// the correct shape for a `dyn Provider`.
    fixes: std::sync::Mutex<LearnedFixes>,
    /// Strip inline `<think>…</think>` blocks from the content stream into
    /// ThinkingDelta (FR-2, `Config.strip_think_tags`, default on).
    strip_think_tags: bool,
    /// `Config.extra_body` / `Config.extra_headers`, applied to every
    /// request (see the `with_extra_*` builders).
    extras: super::RequestExtras,
}

impl OpenAi {
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
            fixes: std::sync::Mutex::new(LearnedFixes::default()),
            strip_think_tags: true,
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

    /// The FR-2 knob (`Config::strip_think_tags`): wired by
    /// `provider::build`; the `new()` default is on, matching the config
    /// default.
    pub fn with_strip_think_tags(mut self, on: bool) -> Self {
        self.strip_think_tags = on;
        self
    }

    /// Preset gateway fixes learned in a PREVIOUS process (FR-3):
    /// `provider::build` feeds `Config::gateway_fixes_preset` here so the
    /// very first request already carries the adjusted body shape — the
    /// teaching 400 (and its wasted round-trip) is skipped entirely. Bits the
    /// host didn't preset stay false; learning continues on top (a fix that
    /// fires again just re-records the same bit).
    pub fn with_preset(self, preset: super::GatewayFixes) -> Self {
        if preset != super::GatewayFixes::default() {
            let mut fixes = self.fixes.lock().unwrap_or_else(|p| p.into_inner());
            fixes.max_completion_tokens |= preset.max_completion_tokens;
            fixes.no_stream_options |= preset.no_stream_options;
            fixes.no_reasoning_effort |= preset.no_reasoning_effort;
            fixes.no_reasoning_echo |= preset.no_reasoning_echo;
        }
        self
    }

    /// One internal Message may expand to several wire messages
    /// (tool results become separate `tool` role messages). `echo_reasoning`
    /// false sends assistant messages without their carried reasoning — the
    /// shape every request had before reasoning carry, and the one used once
    /// this gateway taught us it rejects the field (see `BodyFix::
    /// DropReasoningEcho`).
    fn message_jsons(m: &Message, echo_reasoning: bool) -> Vec<Value> {
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
                // Reasoning carry, OpenAI-compatible side
                // (`Config.preserved_reasoning`): hand the turn's
                // `reasoning_content` back on the assistant message, the way
                // the gateway gave it — the DeepSeek / GLM contract for a
                // model to keep its reasoning across a tool-use loop. Only
                // this family's state, and only when there is text to send.
                if echo_reasoning {
                    if let Some(r) = m.reasoning_for(super::REASONING_OPENAI) {
                        let carried: Vec<&str> = r
                            .blocks
                            .iter()
                            .map(|b| b.text.as_str())
                            .filter(|t| !t.is_empty())
                            .collect();
                        if !carried.is_empty() {
                            msg["reasoning_content"] = json!(carried.join("\n"));
                        }
                    }
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
                        // Unreachable in practice: `chat` rejects document
                        // blocks before building any body (see
                        // `reject_documents`). Defensive depth only — if a
                        // future path skips that gate, the request still says
                        // the document was omitted rather than silently
                        // dropping it.
                        Block::Document { .. } => parts.push(json!({
                            "type": "text",
                            "text": "[document block omitted: not supported by this provider]"
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

/// A single request-body fix for a 400 the gateway returns when it doesn't
/// accept a field the spec/model emits. Distinct fields → composable fixes.
#[derive(Clone, Copy)]
enum BodyFix {
    /// Rename `max_tokens` → `max_completion_tokens` (reasoning models).
    MaxCompletionTokens,
    /// Omit `stream_options` (old gateways/vLLM); usage then unreported.
    DropStreamOptions,
    /// Omit `reasoning_effort` (old gateways don't know it).
    DropReasoningEffort,
    /// Strip `reasoning_content` off every assistant message (gateways that
    /// reject the echo of a previous turn's reasoning). Only ever relevant
    /// while `Config.preserved_reasoning` is on.
    DropReasoningEcho,
}

/// Fixes already learned for this gateway, applied proactively each turn so the
/// 400 that taught them isn't re-triggered on every subsequent request.
/// `Copy`: `chat` snapshots it once per request — the echo decision (message
/// building) and `apply_learned` (body finishing) must read the same bits.
#[derive(Default, Clone, Copy)]
struct LearnedFixes {
    max_completion_tokens: bool,
    no_stream_options: bool,
    no_reasoning_effort: bool,
    no_reasoning_echo: bool,
}

/// Streaming `<think>…</think>` separator for the openai-compat content
/// channel (宿主集成需求单 FR-2 缩减版): vLLM-family deployments without a
/// reasoning parser inline the reasoning trace in `delta.content` wrapped in
/// literal think tags. This machine splits that stream into the TEXT channel
/// and the THINKING channel — pieces land as `(is_text, chunk)`, the caller
/// maps thinking onto the existing `ChatEvent::ThinkingDelta` (the standing
/// rule: thinking never enters history or the reply text).
///
/// Tags are matched across arbitrary chunk boundaries (partial-tag prefixes
/// are held back, at most one tag's worth of bytes). Once a complete
/// `<think>` is seen the stream is committed to think-until-close: a stream
/// that ends mid-block flushes the remainder as THINKING — fail-closed for
/// text purity (no tag residue can ever reach `text`/history); unterminated
/// partial tags held in Normal state flush as text at end-of-stream (they
/// were never a tag). Byte-exact tags only: prose that legitimately quotes
/// these tags must turn the knob off (`Config::strip_think_tags`). A close
/// tag with no open one is ordinary text.
#[derive(Default)]
struct ThinkStripper {
    state: ThinkState,
    buf: String,
}

#[derive(Default, PartialEq)]
enum ThinkState {
    #[default]
    Normal,
    InThink,
}

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

impl ThinkStripper {
    fn feed(&mut self, chunk: &str) -> Vec<(bool, String)> {
        self.buf.push_str(chunk);
        self.drain()
    }

    /// Flush at end-of-stream: Normal-state leftovers were never a tag (text);
    /// leftovers inside an unterminated `<think>` block stay thinking.
    fn finish(&mut self) -> Vec<(bool, String)> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        vec![(
            self.state == ThinkState::Normal,
            std::mem::take(&mut self.buf),
        )]
    }

    fn drain(&mut self) -> Vec<(bool, String)> {
        let mut out = Vec::new();
        loop {
            match self.state {
                ThinkState::Normal => match self.buf.find('<') {
                    Some(i) => {
                        if i > 0 {
                            out.push((true, self.buf.drain(..i).collect()));
                        }
                        // buf now starts with '<'.
                        if self.buf.starts_with(THINK_OPEN) {
                            self.buf.drain(..THINK_OPEN.len());
                            self.state = ThinkState::InThink;
                        } else if THINK_OPEN.starts_with(self.buf.as_str()) {
                            break; // a possible tag prefix — wait for more bytes
                        } else {
                            out.push((true, self.buf.drain(..1).collect()));
                        }
                    }
                    None => {
                        if !self.buf.is_empty() {
                            out.push((true, std::mem::take(&mut self.buf)));
                        }
                        break;
                    }
                },
                ThinkState::InThink => match self.buf.find(THINK_CLOSE) {
                    Some(i) => {
                        if i > 0 {
                            out.push((false, self.buf.drain(..i).collect()));
                        }
                        self.buf.drain(..THINK_CLOSE.len());
                        self.state = ThinkState::Normal;
                    }
                    None => {
                        // Hold back a suffix that could be a partial close tag.
                        let hold = (1..THINK_CLOSE.len())
                            .rev()
                            .find(|k| self.buf.ends_with(&THINK_CLOSE[..*k]))
                            .unwrap_or(0);
                        let cut = self.buf.len() - hold;
                        if cut > 0 {
                            out.push((false, self.buf.drain(..cut).collect()));
                        }
                        break;
                    }
                },
            }
        }
        out
    }
}

impl LearnedFixes {
    fn record(&mut self, fix: BodyFix) {
        match fix {
            BodyFix::MaxCompletionTokens => self.max_completion_tokens = true,
            BodyFix::DropStreamOptions => self.no_stream_options = true,
            BodyFix::DropReasoningEffort => self.no_reasoning_effort = true,
            BodyFix::DropReasoningEcho => self.no_reasoning_echo = true,
        }
    }

    /// Export projection (FR-3): the public, serializable bit set — this is
    /// what `gateway_fixes()` reports and what a host persists into the next
    /// process's `Config::gateway_fixes_preset`. Takes `self` by value: four
    /// booleans, `Copy`, and a by-value receiver is the convention for a
    /// `to_*` method on a `Copy` type.
    fn to_fixes(self) -> super::GatewayFixes {
        super::GatewayFixes {
            max_completion_tokens: self.max_completion_tokens,
            no_stream_options: self.no_stream_options,
            no_reasoning_effort: self.no_reasoning_effort,
            no_reasoning_echo: self.no_reasoning_echo,
        }
    }
}

/// Reject histories containing PDF document blocks. The OpenAI-compatible
/// protocol has no `document` content type (file inputs ride the Responses
/// API, not chat completions), so a document can't be forwarded — error
/// explicitly instead of silently dropping content the model was told about.
/// Pure (no HTTP) so it's directly unit-testable; `chat` gates on it first.
fn reject_documents(messages: &[Message]) -> Result<()> {
    let has_doc = messages
        .iter()
        .flat_map(|m| &m.content)
        .any(|b| matches!(b, Block::Document { .. }));
    if has_doc {
        // The message is written to be surfaced RAW to an end user (frontend
        // provider-gating missed the attachment → the user reads this), so it
        // says what to DO, in user terms — no host/developer jargon.
        bail!(
            "this endpoint does not support PDF documents (only the Anthropic \
             provider does): convert the document to text or images and \
             resend, or use the Anthropic provider"
        );
    }
    Ok(())
}

/// Does this request body echo reasoning on any assistant message
/// (`reasoning_content` present)? The no-op guard for
/// `BodyFix::DropReasoningEcho` — same discipline as the other fixes: only
/// diagnose a fix the body actually has something to undo.
fn body_echoes_reasoning(body: &Value) -> bool {
    body["messages"]
        .as_array()
        .is_some_and(|msgs| msgs.iter().any(|m| m.get("reasoning_content").is_some()))
}

/// The replayable form of a response's `reasoning_content` so far: one block
/// that preceded every content block (`before: 0` — chat-completions streams
/// the reasoning trace ahead of `content` and `tool_calls`, so "first" is
/// both its real and its only position). `None` when none arrived.
fn reasoning_so_far(text: &str) -> Option<super::Reasoning> {
    if text.is_empty() {
        return None;
    }
    super::into_reasoning(
        super::REASONING_OPENAI,
        vec![super::ReasoningBlock {
            before: 0,
            text: text.to_string(),
            opaque: json!({}),
        }],
    )
}

/// Remove `reasoning_content` from every message of a chat body.
fn strip_reasoning_echo(obj: &mut serde_json::Map<String, Value>) {
    if let Some(Value::Array(msgs)) = obj.get_mut("messages") {
        for m in msgs {
            if let Some(o) = m.as_object_mut() {
                o.remove("reasoning_content");
            }
        }
    }
}

impl Provider for OpenAi {
    fn chat(&self, req: &ChatRequest, on_event: &mut dyn FnMut(ChatEvent)) -> Result<ChatResponse> {
        // Cancel checkpoint before the first POST — the anthropic side has
        // had one all along (external review P3, symmetry): the agent's
        // loop-head check runs before the turn starts, but a cancel landing
        // during the previous turn's tool phase would otherwise buy one full
        // request round-trip (connect + upload + TTFB) before
        // CancellableSse's in-stream check notices. Same guard shape and
        // message style as the anthropic twin and the 400-resend checkpoint
        // below; the run-level retry ladder treats the resulting Err + set
        // flag as EXIT_CANCELLED.
        if req.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("request cancelled before sending to the OpenAI-compatible API");
        }
        // Fail fast (before any body building or HTTP) if the history carries a
        // PDF document block: the chat-completions protocol has no document
        // content block, so the only honest options are an explicit error or
        // silently dropping the document — we error. Deterministic; no tokens
        // are burned because nothing is sent.
        reject_documents(req.messages)?;
        let mut messages: Vec<Value> = Vec::new();
        if let Some(sys) = req.system {
            messages.push(json!({"role": "system", "content": sys}));
        }
        // Snapshot the learned fixes ONCE per request (Copy): the message
        // building below (does the gateway take `reasoning_content`?) and
        // `apply_learned` further down must agree — reading the mutex twice
        // would let a fix recorded in between produce a half-adjusted body.
        let learned = self.snapshot_fixes();
        for m in req.messages {
            messages.extend(Self::message_jsons(m, !learned.no_reasoning_echo));
        }

        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": messages,
        });
        // Forward the reasoning-effort knob verbatim. NOT validated: every
        // vendor's ladder differs and shifts across generations, so a fixed enum
        // here would lag reality. An old gateway that doesn't know the field
        // will 400 — the diagnose_400/apply_fix path drops it (and records the
        // fix so the next turn omits it up front).
        if let Some(eff) = req.reasoning_effort {
            body["reasoning_effort"] = json!(eff);
        }
        // Per-request sampling temperature (LD-4): forwarded only when the
        // caller set it — the agent's turns never do, so their bodies are
        // unchanged. OpenAI-compatible endpoints universally know the field
        // (unlike reasoning_effort), so no 400-fix arm is wired for it.
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if !req.tools.is_empty() {
            body["tools"] = Self::tools_json(req.tools);
        }
        // Config extras, BEFORE the learned fixes: a fix learned from a 400
        // about a field that only the extras supply (say `reasoning_effort`
        // spelled by a relay) must still stick on later turns — merging after
        // would re-add it every time.
        self.extras.merge_body(&mut body);
        // Apply fixes learned from prior turns' 400s proactively: a reasoning
        // model on an old gateway otherwise 400s on `max_tokens` every single
        // turn (and eats the retry round-trip each time). Learned fixes are
        // sticky for the provider's lifetime — a gateway's quirks don't change
        // mid-session.
        Self::apply_learned(&mut body, &learned);

        // The per-request timeout is a BUDGET for the whole chat() call, not
        // a fresh allowance per HTTP round-trip (external re-review P2): a
        // 400-diagnose-resend that reuses the original timeout could exceed
        // the caller's budget by up to 3× (each resend getting a full window
        // while the run deadline marches on). One absolute deadline; every
        // resend below spends only what remains of it.
        let chat_deadline = req
            .per_request_timeout
            .map(|t| std::time::Instant::now() + t);
        let remaining =
            || chat_deadline.map(|d| d.saturating_duration_since(std::time::Instant::now()));

        let mut resp = self.post(&body, req.per_request_timeout)?;

        // A 400 may name a field this gateway doesn't accept (newer reasoning
        // models reject `max_tokens`; older gateways/vLLM reject the unknown
        // `stream_options` / `reasoning_effort`; some reject `reasoning_content`
        // echoed back on assistant messages). Diagnose, apply the fix, and
        // RECORD it so the next turn's body is already fixed. At most four
        // turns of the loop (four distinct fields); when no known fix applies
        // the 400 bails with the text.
        //
        // RECORD-ON-SUCCESS: fixes are persisted only after the adjusted body
        // gets a 2xx (or the call fails transiently and records nothing). A
        // fix is proof "the gateway rejects the FIELD", and only a subsequent
        // success is that proof. Recording at apply time let a VALUE error
        // poison the table: a 400 saying `max_tokens must be <= 100` matches
        // the rename rule, the retry fails on `max_completion_tokens` with
        // text no rule matches → bail — but the rename was already learned,
        // so every later turn proactively sends a body the gateway rejects
        // for the same value reason (and the 400s it triggers are of the
        // unfixable kind).
        let mut applied: Vec<BodyFix> = Vec::new();
        for _ in 0..4 {
            if resp.status() != reqwest::StatusCode::BAD_REQUEST {
                break;
            }
            // Capped (mcp.rs MCP_ERR_BODY_MAX twin): the 400 body's size is
            // the gateway's choice and only feeds diagnosis + the error
            // message — never buffered whole.
            let text = super::capped_err_body(&mut resp);
            match Self::diagnose_400(&text, &body) {
                Some(fix) => {
                    // Cancel checkpoint before the resend (external re-review
                    // P2): the 400 round-trip may have outlived the user's
                    // patience — don't start another HTTP request the caller
                    // already vetoed. The run-level retry ladder treats the
                    // resulting Err + set flag as EXIT_CANCELLED.
                    if req.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        anyhow::bail!("request cancelled while adjusting for a 400");
                    }
                    Self::apply_fix(&mut body, fix);
                    applied.push(fix);
                    resp = self
                        .post(&body, remaining())
                        .context("retry after adjusting request for 400 failed")?;
                }
                None => {
                    // Structured status + historical display text
                    // (see ApiStatusError) — plus the carry hint when the 400
                    // is the "reasoning trace missing" class (a body edit
                    // can't repair what was never kept).
                    let mut msg = format!("OpenAI-compatible API error 400: {text}");
                    if !body_echoes_reasoning(&body) && text.contains("reasoning_content") {
                        msg.push_str(super::REASONING_CARRY_HINT);
                    }
                    return Err(super::ApiStatusError::new(400, msg).into());
                }
            }
        }

        if !resp.status().is_success() {
            let status = resp.status();
            // Capped (mcp.rs MCP_ERR_BODY_MAX twin): an unbounded read here
            // let a hostile/broken gateway OOM us with an endless "error
            // body" embedded in the diagnostic.
            let text = super::capped_err_body(&mut resp);
            // Structured status + historical display text (see ApiStatusError).
            return Err(super::ApiStatusError::new(
                status.as_u16(),
                format!("OpenAI-compatible API error {status}: {text}"),
            )
            .into());
        }
        // The gateway accepted the (possibly adjusted) body — now the applied
        // fixes are proven to be about field acceptance, persist them.
        if !applied.is_empty() {
            // Poison-tolerant lock (mcp.rs precedent): this critical section
            // has no panic path, so into_inner is safe — and a poisoning
            // elsewhere can't cascade into a second panic here.
            let mut learned = self.fixes.lock().unwrap_or_else(|e| e.into_inner());
            for fix in applied {
                learned.record(fix);
            }
        }

        // Captured BEFORE the body is consumed (the headers die with the
        // response once CancellableSse owns it) — the zero-data truncation
        // diagnosis below names the content-type so a gateway HTML error
        // page is identifiable from the transcript alone.
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let mut sse = CancellableSse::new(resp, req.cancel.clone(), self.idle)?;
        Self::parse_loop(
            &mut sse,
            on_event,
            self.strip_think_tags,
            content_type.as_deref(),
        )
    }

    fn gateway_fixes(&self) -> super::GatewayFixes {
        // Same poison-tolerant lock as the two sites in `chat`.
        self.fixes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .to_fixes()
    }
}

impl OpenAi {
    /// The learned fixes as of now. Poison-tolerant lock (mcp.rs precedent):
    /// nothing under the lock can panic, so a poisoning elsewhere can't
    /// cascade into a second panic here.
    fn snapshot_fixes(&self) -> LearnedFixes {
        *self
            .fixes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn post(
        &self,
        body: &Value,
        timeout: Option<std::time::Duration>,
    ) -> Result<reqwest::blocking::Response> {
        let rb = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json");
        let mut rb = self.extras.apply_headers(rb);
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
    /// - `reasoning_effort` → drop it (old gateways don't know the field).
    /// - `reasoning_content` → strip it off the assistant messages (gateways
    ///   that reject an echoed reasoning trace).
    ///
    /// Thin wrapper over `diagnose_400` + `apply_fix` so the live path can
    /// RECORD the specific fix (to skip the 400 next turn) while this stays a
    /// pure function the tests assert against. Returns `None` for an unknown 400.
    #[cfg(test)]
    fn body_fix_for_400(text: &str, body: &Value) -> Option<Value> {
        let fix = Self::diagnose_400(text, body)?;
        let mut next = body.clone();
        Self::apply_fix(&mut next, fix);
        Some(next)
    }

    /// Classify a 400 into the single field fix it implies, iff that field is
    /// actually present in the body (so a fix never no-ops — `apply_fix` would
    /// change nothing and the loop would loop forever on the same 400).
    fn diagnose_400(text: &str, body: &Value) -> Option<BodyFix> {
        if text.contains("max_tokens") && body.get("max_tokens").is_some() {
            return Some(BodyFix::MaxCompletionTokens);
        }
        if text.contains("stream_options") && body.get("stream_options").is_some() {
            return Some(BodyFix::DropStreamOptions);
        }
        if text.contains("reasoning_effort") && body.get("reasoning_effort").is_some() {
            return Some(BodyFix::DropReasoningEffort);
        }
        // A 400 naming `reasoning_content` while the body echoes some: the
        // gateway takes no reasoning on assistant messages. (A 400 about
        // `reasoning_effort` never reads "reasoning_content", so the two
        // rules can't shadow each other.)
        if text.contains("reasoning_content") && body_echoes_reasoning(body) {
            return Some(BodyFix::DropReasoningEcho);
        }
        None
    }

    /// Apply one fix to a body. `body` is always a JSON object (the chat body).
    fn apply_fix(body: &mut Value, fix: BodyFix) {
        let obj = match body.as_object_mut() {
            Some(o) => o,
            None => return,
        };
        match fix {
            BodyFix::MaxCompletionTokens => {
                if let Some(mt) = obj.remove("max_tokens") {
                    obj.insert("max_completion_tokens".into(), mt);
                }
            }
            BodyFix::DropStreamOptions => {
                obj.remove("stream_options");
            }
            BodyFix::DropReasoningEffort => {
                obj.remove("reasoning_effort");
            }
            BodyFix::DropReasoningEcho => strip_reasoning_echo(obj),
        }
    }

    /// Apply ALL fixes learned from prior 400s to a fresh per-turn body, so the
    /// first request of the turn is already adjusted (no wasted 400 round-trip).
    fn apply_learned(body: &mut Value, learned: &LearnedFixes) {
        let obj = match body.as_object_mut() {
            Some(o) => o,
            None => return,
        };
        if learned.max_completion_tokens {
            if let Some(mt) = obj.remove("max_tokens") {
                obj.insert("max_completion_tokens".into(), mt);
            }
        }
        if learned.no_stream_options {
            obj.remove("stream_options");
        }
        if learned.no_reasoning_effort {
            obj.remove("reasoning_effort");
        }
        // Belt and braces: `chat` already builds the messages without the
        // echo under this bit; a preset or a concurrent learner makes this the
        // single place the guarantee is kept regardless.
        if learned.no_reasoning_echo {
            strip_reasoning_echo(obj);
        }
    }

    /// Assemble a full response from an SSE byte stream (sync, for tests).
    #[cfg(test)]
    fn parse_stream<R: std::io::Read>(
        reader: R,
        on_event: &mut dyn FnMut(ChatEvent),
    ) -> Result<ChatResponse> {
        let mut sse = SseReader::new(reader);
        Self::parse_loop(&mut sse, on_event, /* strip_think_tags */ true, None)
    }

    /// Streaming core shared by the sync test path (`SseReader`) and the live
    /// path (`CancellableSse`). A cancel/idle abort surfaces as a stop_reason
    /// carrying whatever partial text/calls arrived — mirroring stream_truncated,
    /// so the agent ends the run cleanly instead of hanging on a half-open
    /// connection. `content_type` rides along for the zero-data truncation
    /// diagnosis below (external review P3: a 200 whose body is a gateway
    /// HTML error page used to end the run as a silent empty `stream_truncated`).
    fn parse_loop<S: SseSource>(
        src: &mut S,
        on_event: &mut dyn FnMut(ChatEvent),
        strip_think_tags: bool,
        content_type: Option<&str>,
    ) -> Result<ChatResponse> {
        let mut text = String::new();
        let mut calls: std::collections::BTreeMap<u64, PendingCall> = Default::default();
        let mut cur_idx: u64 = 0;
        let mut finish_reason = String::from("stop");
        let mut got_finish = false;
        // Any `data:` payload dispatched at all — the zero-data marker for
        // the truncation diagnosis (a real truncation mid-stream has
        // payloads; a non-SSE body has none).
        let mut saw_payload = false;
        let mut abort: Option<&'static str> = None;
        let mut usage: Option<Usage> = None;
        // Reasoning carry: the turn's `reasoning_content` deltas, joined.
        // Display still goes through ThinkingDelta per delta; this copy is
        // the replayable state the agent may keep (`Config.preserved_reasoning`).
        let mut reasoning_text = String::new();
        // FR-2: `<think>` splitter across the content channel. `None` = knob
        // off, the raw passthrough shape (byte-identical to pre-FR-2).
        let mut stripper = strip_think_tags.then(ThinkStripper::default);
        // Every loop exit flushes the splitter's held-back bytes into their
        // channels BEFORE the partial/total assembly — text-only bytes reach
        // `text` (history), thinking bytes only fire ThinkingDelta events.
        // Covers the clean ends AND the abort/error arms (honest-cost
        // partials keep every received byte visible).
        macro_rules! flush_think {
            () => {
                for (is_text, piece) in stripper.as_mut().map(|s| s.finish()).unwrap_or_default() {
                    if is_text {
                        text.push_str(&piece);
                        on_event(ChatEvent::TextDelta(piece));
                    } else {
                        on_event(ChatEvent::ThinkingDelta(piece));
                    }
                }
            };
        }

        loop {
            let data = match src.next_payload() {
                Ok(Some(d)) => {
                    saw_payload = true;
                    d
                }
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
                // assemble a partial response and report it as success.
                // Mirror the Anthropic honest-cost path instead: partial
                // blocks + a marker Text block + stop_reason "stream_error",
                // carrying whatever usage already arrived — the agent bills
                // the partial cost and maps this to a retryable Err. Bailing
                // here dropped the usage (gateways that attach usage to the
                // last choice chunk may have already sent it).
                Err(StreamAbort::StreamError(msg)) => {
                    flush_think!();
                    let mut blocks = Self::assemble_blocks(&text, &calls);
                    blocks.push(Block::Text(format!(
                        "[stream error] OpenAI stream read error: {msg}"
                    )));
                    return Ok(ChatResponse {
                        reasoning: reasoning_so_far(&reasoning_text),
                        blocks,
                        stop_reason: "stream_error".into(),
                        usage,
                    });
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
            // that as no error (filter out null). Same honest-cost shape as
            // the read-failure arm above.
            if let Some(err) = ev.get("error").filter(|v| !v.is_null()) {
                flush_think!();
                let mut blocks = Self::assemble_blocks(&text, &calls);
                blocks.push(Block::Text(format!(
                    "[stream error] OpenAI stream error: {}",
                    err["message"].as_str().unwrap_or("(no message)")
                )));
                return Ok(ChatResponse {
                    reasoning: reasoning_so_far(&reasoning_text),
                    blocks,
                    stop_reason: "stream_error".into(),
                    usage,
                });
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
                    // OpenAI-compat reports the reasoning share of the
                    // completion separately (`completion_tokens_details.
                    // reasoning_tokens`); a subset split of output_tokens,
                    // never an extra axis — 0 when the gateway doesn't say.
                    reasoning_tokens: u["completion_tokens_details"]["reasoning_tokens"]
                        .as_u64()
                        .unwrap_or(0),
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
                // FR-2: inline `<think>` blocks split off the content channel
                // (knob on) — text pieces keep the old path, thinking pieces
                // fire ThinkingDelta only (never text/history). Knob off →
                // the raw passthrough, byte-identical to pre-FR-2.
                match stripper.as_mut() {
                    Some(st) => {
                        for (is_text, piece) in st.feed(t) {
                            if is_text {
                                text.push_str(&piece);
                                on_event(ChatEvent::TextDelta(piece));
                            } else {
                                on_event(ChatEvent::ThinkingDelta(piece));
                            }
                        }
                    }
                    None => {
                        text.push_str(t);
                        on_event(ChatEvent::TextDelta(t.to_string()));
                    }
                }
            }
            // Reasoning models stream the thinking trace to the UI. Surface it as
            // a ThinkingDelta event ONLY — never into `text`/blocks: the reply is
            // `content`, and the trace is not part of it. What the trace CAN do
            // is ride back as replayable state (reasoning carry, the agent's
            // decision) — and only the vendor-native field qualifies: a
            // gateway that sends `reasoning_content` is speaking the
            // DeepSeek/GLM protocol whose assistant messages take it back,
            // whereas OpenRouter's normalized `reasoning` is a display
            // projection with no documented input twin, so it stays
            // display-only. Field absent → non-reasoning model, zero behavior
            // change.
            //
            // The field name is NOT standardized across OpenAI-compatible gateways:
            //   - DeepSeek-R1 / GLM / o-series (direct) → delta.reasoning_content
            //   - OpenRouter (normalizes every vendor's thinking) → delta.reasoning
            // Try the vendor-native field first, fall back to OpenRouter's name.
            // `reasoning_details` (OpenRouter's structured array) is intentionally
            // ignored: `reasoning` is its plain-text projection, enough for the UI.
            // An EMPTY `reasoning_content` (some gateways emit it as a placeholder
            // on the first chunk) must NOT shadow a non-empty `reasoning`: filter
            // empties before the fallback so we still pick up the real trace.
            if let Some(r) = delta["reasoning_content"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| delta["reasoning"].as_str().filter(|s| !s.is_empty()))
            {
                on_event(ChatEvent::ThinkingDelta(r.to_string()));
                if delta["reasoning_content"].as_str() == Some(r) {
                    reasoning_text.push_str(r);
                }
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
                        // Surface the fragment as it lands: the assembled
                        // input only exists when the stream ends, but a UI
                        // wants "constructing call…" rendered live. id/name
                        // ride the same entry (a call's first delta carries
                        // them), so parallel same-name calls stay pairable.
                        if !args.is_empty() {
                            on_event(ChatEvent::ToolInputDelta {
                                id: entry.id.clone(),
                                name: entry.name.clone(),
                                delta: args.to_string(),
                            });
                        }
                    }
                }
            }
        }

        flush_think!();
        let mut blocks = Self::assemble_blocks(&text, &calls);

        // Zero-data truncation diagnosis: a 200 whose body was never SSE (a
        // gateway error page) lands here with zero payloads, empty blocks and
        // no finish_reason — the bare `stream_truncated` label said THAT it
        // failed, never WHY. Mirror the stream_error arm's marker-block
        // style: point at the root cause (content-type is the giveaway),
        // honest-cost style, without changing the stop_reason/exit contract.
        // Clean-EOF exits ONLY (wave-2 rework): an abort (cancel/idle/error)
        // with zero payloads has its own honest label — stamping
        // "non-streaming body?" over a cancel (e.g. the flag landing in the
        // TTFB window) would put a false accusation into history, final_text
        // and the session.
        if abort.is_none() && !got_finish && !saw_payload {
            let mut diag = "[stream error] no SSE data events in response".to_string();
            if let Some(ct) = content_type {
                diag.push_str(&format!(" (content-type: {ct})"));
            }
            diag.push_str(" — gateway returned a non-streaming body?");
            blocks.push(Block::Text(diag));
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
        Ok(ChatResponse {
            reasoning: reasoning_so_far(&reasoning_text),
            blocks,
            stop_reason,
            usage,
        })
    }

    /// Assemble the parsed blocks — streamed text (if any) then tool calls in
    /// index order. Shared by parse_loop's tail and its mid-stream error arms,
    /// so an honest-cost partial response carries the same block shape as a
    /// normal finish.
    fn assemble_blocks(
        text: &str,
        calls: &std::collections::BTreeMap<u64, PendingCall>,
    ) -> Vec<Block> {
        let mut blocks: Vec<Block> = Vec::new();
        if !text.is_empty() {
            blocks.push(Block::Text(text.to_string()));
        }
        for c in calls.values() {
            let input = if c.arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&c.arguments).unwrap_or_else(|_| json!({"_raw": c.arguments}))
            };
            blocks.push(Block::ToolUse {
                id: c.id.clone(),
                name: c.name.clone(),
                input,
            });
        }
        blocks
    }
}

#[cfg(test)]
#[path = "openai_review_pins.rs"]
mod review_pins;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

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
    fn body_fix_drops_reasoning_effort() {
        // Older OpenAI-compatible gateways / vLLM don't know `reasoning_effort`
        // and 400 on it — drop and retry, same drop-and-retry as stream_options.
        let body = json!({"model": "m", "max_tokens": 100, "reasoning_effort": "high"});
        let fixed =
            OpenAi::body_fix_for_400("Unrecognized request argument: reasoning_effort", &body)
                .unwrap();
        assert!(fixed.get("reasoning_effort").is_none());
        assert_eq!(fixed.get("max_tokens"), Some(&json!(100))); // untouched
    }

    /// A chat body whose assistant message echoes a reasoning trace — the
    /// shape `chat()` builds while reasoning carry is on.
    fn echoing_body() -> Value {
        json!({
            "model": "m",
            "max_tokens": 100,
            "reasoning_effort": "high",
            "messages": [
                {"role": "user", "content": "q"},
                {"role": "assistant", "content": "a", "reasoning_content": "trace one"},
                {"role": "assistant", "content": "b", "reasoning_content": "trace two"},
            ],
        })
    }

    #[test]
    fn body_fix_strips_the_reasoning_echo() {
        // A gateway that takes no `reasoning_content` on assistant messages
        // 400s naming it — strip the field from EVERY message and retry; the
        // message order, contents and every other field stay untouched.
        let fixed = OpenAi::body_fix_for_400(
            "Invalid value for 'messages[1]': reasoning_content is not supported",
            &echoing_body(),
        )
        .unwrap();
        let msgs = fixed["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        for m in msgs {
            assert!(m.get("reasoning_content").is_none(), "{m}");
        }
        assert_eq!(msgs[1]["content"], "a");
        assert_eq!(msgs[2]["content"], "b");
        assert_eq!(fixed["reasoning_effort"], "high");
        assert_eq!(fixed["max_tokens"], 100);
    }

    #[test]
    fn body_fix_reasoning_echo_400_without_an_echo_is_not_a_fix() {
        // The no-op guard: the body echoes nothing, so stripping would change
        // nothing and the resend would loop on the same 400. Notably this is
        // the "Missing reasoning_content" class — the endpoint WANTS the
        // trace and the carry switch is off — which no body fix can repair.
        let body = json!({"model": "m", "max_tokens": 100, "messages": [
            {"role": "user", "content": "q"},
            {"role": "assistant", "content": "a"},
        ]});
        assert!(OpenAi::body_fix_for_400(
            "Missing `reasoning_content` field in the assistant message at message index 1",
            &body
        )
        .is_none());
    }

    #[test]
    fn reasoning_effort_400_does_not_touch_the_echo() {
        // The two rules read different substrings: a 400 about
        // `reasoning_effort` drops that field only — the echo survives.
        let fixed = OpenAi::body_fix_for_400(
            "Unrecognized request argument: reasoning_effort",
            &echoing_body(),
        )
        .unwrap();
        assert!(fixed.get("reasoning_effort").is_none());
        assert_eq!(fixed["messages"][1]["reasoning_content"], "trace one");
    }

    #[test]
    fn tool_result_becomes_tool_message() {
        let m = Message::user(vec![Block::ToolResult {
            tool_use_id: "call_1".into(),
            content: "ok".into(),
            image: None,
            is_error: false,
        }]);
        let msgs = OpenAi::message_jsons(&m, false);
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
        let msgs = OpenAi::message_jsons(&m, false);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"][1]["type"], "image_url");
    }

    #[test]
    fn document_blocks_are_rejected_before_any_request() {
        // PDF document blocks are Anthropic-only: chat-completions has no
        // document content type, so `chat` must fail fast (no HTTP, no burned
        // tokens) with an explicit error naming the limitation — silently
        // dropping the document would hide content the model was told about.
        let doc = || {
            vec![Message::user(vec![
                Block::Document {
                    media_type: "application/pdf".into(),
                    data: "JVBERi0=".into(),
                },
                Block::Text("summarize".into()),
            ])]
        };
        let err = reject_documents(&doc()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Anthropic"), "message names the fix: {msg}");
        // The message is user-actionable (frontends surface it raw): it says
        // what to do, in user terms — pinned so a future rewording doesn't
        // regress into developer jargon.
        assert!(
            msg.contains("convert the document to text or images"),
            "message carries a user-actionable fix: {msg}"
        );
        // Clean histories pass — text, images, tool traffic.
        let img_only = [Message::user(vec![Block::Image {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        }])];
        assert!(reject_documents(&img_only).is_ok());
        assert!(reject_documents(&[]).is_ok());
    }

    #[test]
    fn document_block_in_message_jsons_is_an_omission_placeholder() {
        // Defensive depth only (chat's reject_documents gate runs first): if a
        // future path skips that gate, the wire still SAYS the document was
        // omitted rather than silently dropping it.
        let m = Message::user(vec![Block::Document {
            media_type: "application/pdf".into(),
            data: "JVBERi0=".into(),
        }]);
        let msgs = OpenAi::message_jsons(&m, false);
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert!(msgs[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("omitted"));
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
        let msgs = OpenAi::message_jsons(&m, false);
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
    fn stream_tool_input_deltas_pair_fragments_with_call_ids() {
        // Argument fragments must surface as ToolInputDelta as they land,
        // carrying the call's id+name so a UI can pair fragments across
        // parallel same-name calls. The c1 fragments arrive split across
        // deltas that carry no id/name of their own — the accumulated entry
        // supplies them. Empty fragments (the first delta's `arguments:""`)
        // are filtered, not emitted.
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"shell\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"comma\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"c2\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"nd\\\":\\\"ls\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"tool_calls\",\"delta\":{}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut events = Vec::new();
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |e| events.push(e)).unwrap();
        assert_eq!(resp.stop_reason, "tool_use");
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
                ("c1".into(), "shell".into(), "{\"comma".into()),
                (
                    "c2".into(),
                    "read_file".into(),
                    "{\"path\":\"a.txt\"}".into()
                ),
                ("c1".into(), "shell".into(), "nd\":\"ls\"}".into()),
            ],
            "fragments in arrival order, each tagged with its call id+name"
        );
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

    /// Yields its prefix bytes, then fails with a connection-reset error
    /// instead of a clean EOF — stands in for a mid-stream connection drop.
    struct FailingAfter {
        data: Vec<u8>,
        pos: usize,
    }
    impl std::io::Read for FailingAfter {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.data.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "connection reset mid-stream",
                ));
            }
            let n = std::cmp::min(b.len(), self.data.len() - self.pos);
            b[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn stream_read_error_returns_partial_cost_honestly() {
        // P2-10: a mid-stream read failure used to bail!, dropping the usage
        // the gateway had already reported (Anthropic's shape already returns
        // Ok with a "[stream error] " marker). Now both halves survive:
        // partial text + carried usage + stop_reason "stream_error", which
        // agent.rs extracts into a retryable Err that still bills the cost.
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial \"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"text\"}}]}\n\n",
            // vLLM/DeepSeek shape: usage rides the last choice chunk (top-level
            // field next to `choices`, not inside the choice object).
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":7}}\n\n",
        );
        let reader = FailingAfter {
            data: raw.as_bytes().to_vec(),
            pos: 0,
        };
        let resp = OpenAi::parse_stream(reader, &mut |_| {}).unwrap();
        assert_eq!(resp.stop_reason, "stream_error");
        assert_eq!(
            resp.usage.map(|u| (u.input_tokens, u.output_tokens)),
            Some((120, 7))
        );
        assert!(matches!(&resp.blocks[0], Block::Text(t) if t == "partial text"));
        assert!(
            matches!(&resp.blocks[1], Block::Text(t) if t.starts_with("[stream error] OpenAI stream read error"))
        );
    }

    #[test]
    fn stream_error_event_returns_partial_cost_honestly() {
        // Same honest-cost contract for the {"error": …} mid-stream event.
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"half a reply\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":3}}\n\n",
            "data: {\"error\":{\"message\":\"server overloaded mid-generation\"}}\n\n",
        );
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |_| {}).unwrap();
        assert_eq!(resp.stop_reason, "stream_error");
        assert_eq!(
            resp.usage.map(|u| (u.input_tokens, u.output_tokens)),
            Some((42, 3))
        );
        assert!(matches!(&resp.blocks[0], Block::Text(t) if t == "half a reply"));
        assert!(matches!(
            &resp.blocks[1],
            Block::Text(t) if t == "[stream error] OpenAI stream error: server overloaded mid-generation"
        ));
    }

    #[test]
    fn stream_reasoning_content_surfaces_thinking_not_text() {
        // DeepSeek-R1 / GLM / o-series via gateways stream the thinking trace
        // under delta.reasoning_content. It must surface as a ThinkingDelta
        // event AND stay out of the assembled text (the reply is `content`).
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"let me think\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\" more\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut thinking = String::new();
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |e| {
            if let crate::provider::ChatEvent::ThinkingDelta(t) = e {
                thinking.push_str(&t);
            }
        })
        .unwrap();
        assert_eq!(thinking, "let me think more");
        // Reasoning did NOT leak into the reply text / blocks.
        assert_eq!(resp.blocks.len(), 1);
        assert!(matches!(&resp.blocks[0], Block::Text(t) if t == "hi"));
    }

    #[test]
    fn stream_openrouter_reasoning_field_surfaces_thinking() {
        // OpenRouter normalizes EVERY vendor's thinking into delta.reasoning
        // (paired with a reasoning_details array we ignore), NOT reasoning_content.
        // Direct DeepSeek/GLM still use reasoning_content (covered above). Both
        // must surface as ThinkingDelta and stay out of the assembled reply text.
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning\":\"step 1 \",\"reasoning_details\":[{\"type\":\"reasoning.text\"}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"step 2\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut thinking = String::new();
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |e| {
            if let crate::provider::ChatEvent::ThinkingDelta(t) = e {
                thinking.push_str(&t);
            }
        })
        .unwrap();
        assert_eq!(thinking, "step 1 step 2");
        // reasoning + reasoning_details did NOT leak into the reply text / blocks.
        assert_eq!(resp.blocks.len(), 1);
        assert!(matches!(&resp.blocks[0], Block::Text(t) if t == "ok"));
        // …and they are not replayable state either: OpenRouter's normalized
        // field is a display channel with no vendor echo contract (the
        // per-vendor encrypted details ride `reasoning_details`, which are not
        // plain text to hand back). Only the native `reasoning_content` is.
        assert!(resp.reasoning.is_none());
    }

    #[test]
    fn stream_reasoning_content_is_kept_as_replayable_state() {
        // The vendor-native field is the one with an echo contract (DeepSeek /
        // GLM inside a tool loop): its deltas are joined into the response's
        // `reasoning` side field, ahead of every content block, and stay out of
        // `blocks` exactly as before.
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"let me think\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\" more\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |_| {}).unwrap();
        let r = resp.reasoning.expect("reasoning_content must be captured");
        assert_eq!(r.provider, super::super::REASONING_OPENAI);
        assert_eq!(r.blocks.len(), 1);
        assert_eq!(r.blocks[0].before, 0);
        assert_eq!(r.blocks[0].text, "let me think more");
        assert_eq!(r.blocks[0].opaque, json!({}));
        assert_eq!(resp.blocks.len(), 1);
    }

    #[test]
    fn stream_without_reasoning_content_has_no_state() {
        // A plain reply, and one whose thinking arrived as inline `<think>`
        // tags on the content channel (a content convention, stripped for
        // display only): neither leaves replayable state.
        let plain = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = OpenAi::parse_stream(plain.as_bytes(), &mut |_| {}).unwrap();
        assert!(resp.reasoning.is_none());
        assert_eq!(resp.blocks.len(), 1);
        let inline = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"<think>inner</think>ok\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut thinking = String::new();
        let resp = OpenAi::parse_stream(inline.as_bytes(), &mut |e| {
            if let crate::provider::ChatEvent::ThinkingDelta(t) = e {
                thinking.push_str(&t);
            }
        })
        .unwrap();
        assert_eq!(thinking, "inner");
        assert!(resp.reasoning.is_none());
        assert!(matches!(&resp.blocks[0], Block::Text(t) if t == "ok"));
    }

    #[test]
    fn stream_error_keeps_the_reasoning_received_so_far() {
        // Same honest-partial discipline as text and usage: the trace that
        // arrived before the error is not thrown away by the error path.
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"half a thought\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"half a reply\"}}]}\n\n",
            "data: {\"error\":{\"message\":\"server overloaded mid-generation\"}}\n\n",
        );
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |_| {}).unwrap();
        assert_eq!(resp.stop_reason, "stream_error");
        assert_eq!(
            resp.reasoning.map(|r| r.blocks[0].text.clone()),
            Some("half a thought".to_string())
        );
    }

    #[test]
    fn stream_usage_reads_reasoning_tokens() {
        // OpenAI-compat reports the reasoning share of the completion as
        // completion_tokens_details.reasoning_tokens — a subset split of
        // completion_tokens, surfaced on its own axis (never added to the
        // totals). 0 when the gateway doesn't report it (test above).
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"mull\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":50,\"total_tokens\":60,\"completion_tokens_details\":{\"reasoning_tokens\":40}}}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |_| {}).unwrap();
        let u = resp.usage.expect("usage frame present");
        assert_eq!(u.reasoning_tokens, 40);
        assert_eq!(u.output_tokens, 50);
    }

    /// An assistant message carrying reasoning `texts`, tagged for `provider`.
    fn carried(provider: &str, texts: &[&str]) -> Message {
        let mut m = Message::assistant(vec![Block::Text("answer".into())]);
        m.reasoning = Some(super::super::Reasoning {
            provider: provider.into(),
            blocks: texts
                .iter()
                .map(|t| super::super::ReasoningBlock {
                    before: 0,
                    text: (*t).into(),
                    opaque: json!({}),
                })
                .collect(),
        });
        m
    }

    #[test]
    fn message_jsons_echoes_reasoning_content_on_assistant_messages() {
        let m = carried("openai", &["step one", "step two"]);
        let v = OpenAi::message_jsons(&m, true);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["role"], "assistant");
        assert_eq!(v[0]["reasoning_content"], "step one\nstep two");
        assert_eq!(v[0]["content"], "answer");
        // Echo off = the pre-carry wire shape (switch off, or the gateway
        // taught us it rejects the field).
        let v = OpenAi::message_jsons(&m, false);
        assert!(v[0].get("reasoning_content").is_none());
        assert_eq!(v[0]["content"], "answer");
    }

    #[test]
    fn message_jsons_echo_skips_foreign_state_and_empty_text() {
        // Anthropic-tagged state (signed thinking) never goes out on the
        // chat-completions wire.
        let v = OpenAi::message_jsons(&carried("anthropic", &["x"]), true);
        assert!(v[0].get("reasoning_content").is_none());
        // A block with no readable text (redacted-only) has nothing to send.
        let v = OpenAi::message_jsons(&carried("openai", &[""]), true);
        assert!(v[0].get("reasoning_content").is_none());
        // User messages never carry the field, whatever is attached to them.
        let mut u = Message::user(vec![Block::Text("hi".into())]);
        u.reasoning = carried("openai", &["x"]).reasoning;
        let v = OpenAi::message_jsons(&u, true);
        assert_eq!(v[0]["role"], "user");
        assert!(v[0].get("reasoning_content").is_none());
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
    fn stream_error_event_stops_with_marker() {
        // A streaming error arrives with no `choices`; without the explicit
        // check it would be skipped and surface as an empty success. Now it
        // stops the parse with a marker block carrying the server's message
        // (honest-cost shape, see stream_error_event_returns_partial_cost_
        // honestly for the usage-carrying variant).
        let raw = "data: {\"error\":{\"message\":\"content filter\",\"type\":\"content_filter\"}}\n\ndata: [DONE]\n\n";
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |_| {}).unwrap();
        assert_eq!(resp.stop_reason, "stream_error");
        assert!(matches!(
            &resp.blocks[0],
            Block::Text(t) if t == "[stream error] OpenAI stream error: content filter"
        ));
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
        let msgs = OpenAi::message_jsons(&m, false);
        let roles: Vec<&str> = msgs
            .iter()
            .map(|v| v["role"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(roles, vec!["tool", "tool", "user"]);
        // The trailing user message carries both images.
        let img_count = msgs[2]["content"]
            .as_array()
            .map(|c| c.iter().filter(|p| p["type"] == "image_url").count())
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

    #[test]
    fn stream_unknown_finish_reasons_pass_through_raw() {
        // The LlmTool termination contract is a complete-response WHITELIST
        // precisely because this provider forwards unrecognized
        // finish_reasons verbatim (external re-review P1): content_filter,
        // refusal-like gateway vocabularies, and future reasons arrive at
        // the consumer as-is, never normalized into "end_turn". Pin the
        // passthrough so a "helpful" future normalization here cannot
        // silently launder filtered output into complete responses.
        for reason in ["content_filter", "refusal", "some_future_reason"] {
            let raw = format!(
                concat!(
                    "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"yes\"}}}}]}}\n\n",
                    "data: {{\"choices\":[{{\"index\":0,\"finish_reason\":\"{r}\",\"delta\":{{}}}}]}}\n\n",
                    "data: [DONE]\n\n",
                ),
                r = reason,
            );
            let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |_| {}).unwrap();
            assert_eq!(
                resp.stop_reason, reason,
                "unknown finish_reason must arrive verbatim"
            );
        }
    }

    #[test]
    fn stream_eof_without_done_or_finish_reason_is_truncated_and_keeps_content() {
        // P2 test-gap twin of the anthropic side's stream_truncated_flushes_
        // pending: a stream that ends (plain EOF) carrying NEITHER a
        // finish_reason chunk NOR `data: [DONE]` — a proxy drop after
        // flushing its buffer — must be attributed "stream_truncated", never
        // the default "stop"→"end_turn" masquerade (batch/CI treats the
        // response as incomplete instead of success), and the text +
        // tool-call fragments already accumulated must survive into the
        // blocks.
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"half \"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a reply\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"comma\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"nd\\\":\\\"ls\\\"}\"}}]}}]}\n\n",
        );
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |_| {}).unwrap();
        assert_eq!(
            resp.stop_reason, "stream_truncated",
            "no finish_reason and no [DONE] must be attributed as truncation"
        );
        // Accumulated text survives …
        assert!(matches!(&resp.blocks[0], Block::Text(t) if t == "half a reply"));
        // … and the split tool call reassembles (its fragments were complete
        // JSON — only the stream, not the args, was cut).
        match &resp.blocks[1] {
            Block::ToolUse { id, name, input } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "shell");
                assert_eq!(input["command"], "ls");
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        // Contrast: a missing [DONE] ALONE is not truncation — a
        // finish_reason that did arrive still maps normally at EOF, so the
        // attribution keys on the missing finish_reason, not the sentinel.
        let raw_done = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
        );
        let resp2 = OpenAi::parse_stream(raw_done.as_bytes(), &mut |_| {}).unwrap();
        assert_eq!(resp2.stop_reason, "end_turn");
    }

    // ─── HTTP-mock helpers for provider integration tests ───────────────
    //
    // Minimal HTTP/1.1 servers on a loopback port. reqwest::blocking talks
    // HTTP/1.1 to them exactly as it would to a real gateway. Every response
    // carries Content-Length AND `connection: close` so reqwest opens a fresh
    // socket per request (no pool reuse) — exactly one mock accept per request,
    // in arrival order, with no keep-alive ambiguity.

    /// Read one HTTP request's body off `stream` (the shared loopback
    /// reader — see `provider::testhttp`).
    use crate::provider::testhttp::read_http_body;

    /// Loopback OpenAI-compatible mock driven by the REQUEST BODY (not request
    /// position): any request whose body carries the `max_tokens` key gets a
    /// 400 naming it (driving diagnose_400 → MaxCompletionTokens); every other
    /// request gets a minimal success SSE. Records each received body in
    /// arrival order so the test can assert exactly which field names were sent.
    fn max_tokens_400_mock() -> (String, Arc<Mutex<Vec<Value>>>) {
        let (url, bodies, _cancel) = max_tokens_400_mock_with_cancel_flip();
        (url, bodies)
    }

    /// `max_tokens_400_mock` + a server-side cancel flip: the shared flag is
    /// armed AFTER the first request lands, so the pre-POST checkpoint (an
    /// already-set flag fails fast with zero round-trips) doesn't intercept —
    /// this reaches the 400-fix loop's own checkpoint instead.
    fn max_tokens_400_mock_with_cancel_flip() -> (String, Arc<Mutex<Vec<Value>>>, Arc<AtomicBool>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let bodies_c = bodies.clone();
        let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let cancel_c = cancel.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let body = read_http_body(&stream);
                if let Ok(v) = serde_json::from_str::<Value>(&body) {
                    bodies_c.lock().unwrap().push(v);
                }
                // Arm the cancel once the first request has fully landed: the
                // 400 response is coming back to a client that now carries a
                // set flag — the resend checkpoint must see it.
                cancel_c.store(true, std::sync::atomic::Ordering::SeqCst);
                // A 400 iff the body still carries the OLD field name. (The
                // renamed key `max_completion_tokens` does NOT contain the
                // quoted substring `"max_tokens"` — see the test below.)
                let is_400 = body.contains("\"max_tokens\"");
                let (status, ct, payload): (&str, &str, &str) = if is_400 {
                    (
                        "400 Bad Request",
                        "application/json",
                        r#"{"error":{"message":"Unrecognized request argument: max_tokens","type":"invalid_request_error"}}"#,
                    )
                } else {
                    (
                        "200 OK",
                        "text/event-stream",
                        concat!(
                            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n",
                            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                            "data: [DONE]\n\n",
                        ),
                    )
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {ct}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    payload.len(),
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(payload.as_bytes());
                let _ = stream.flush();
            }
        });
        (url, bodies, cancel)
    }

    #[test]
    fn learned_fix_persists_so_the_second_turn_skips_the_400() {
        // The learned-fix cache (Mutex<LearnedFixes>) must persist across turns:
        // a gateway that 400s on `max_tokens` once should NOT re-400 on turn 2,
        // because turn 1's diagnose_400 → record() made turn 2's apply_learned()
        // rename the field up front. Counting requests to a stateful mock proves
        // the whole chain end to end: turn 1 = 1×(400) + 1×(success) = 2 requests;
        // turn 2 = 1×(success, already fixed) = 1 request. If the cache were
        // broken (Mutex/record/apply_learned wiring), turn 2 would re-400 and
        // retry → 4 requests total, failing the count assertion.
        let (url, bodies) = max_tokens_400_mock();
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        let provider = OpenAi::new(client, url, "k", std::time::Duration::from_secs(30));
        let cancel = Arc::new(AtomicBool::new(false));

        // Turn 1: 400 on `max_tokens` → diagnose_400 → rename to
        // `max_completion_tokens` → record → retry succeeds.
        let m1 = vec![Message::user(vec![Block::Text("turn one".into())])];
        let req1 = ChatRequest {
            system: None,
            messages: &m1,
            tools: &[],
            model: "m",
            max_tokens: 100,
            per_request_timeout: Some(std::time::Duration::from_secs(10)),
            cancel: cancel.clone(),
            cache: true,
            reasoning_effort: None,
            temperature: None,
        };
        let r1 = provider
            .chat(&req1, &mut |_| {})
            .expect("turn 1 recovers after fix");
        assert_eq!(r1.stop_reason, "end_turn");

        // Turn 2: body already carries `max_completion_tokens` (apply_learned
        // fired at the top of chat()) → the gateway does NOT 400.
        let m2 = vec![Message::user(vec![Block::Text("turn two".into())])];
        let req2 = ChatRequest {
            system: None,
            messages: &m2,
            tools: &[],
            model: "m",
            max_tokens: 100,
            per_request_timeout: Some(std::time::Duration::from_secs(10)),
            cancel: cancel.clone(),
            cache: true,
            reasoning_effort: None,
            temperature: None,
        };
        let r2 = provider.chat(&req2, &mut |_| {}).expect("turn 2 succeeds");
        assert_eq!(r2.stop_reason, "end_turn");

        let received = bodies.lock().unwrap().clone();
        // THE cross-turn assertion: exactly 3 requests (turn 1's 400 + retry,
        // turn 2's lone success). A broken cache makes turn 2 re-400 → 4.
        assert_eq!(
            received.len(),
            3,
            "learned fix should make turn 2 skip the 400; saw {} requests",
            received.len()
        );
        // Turn 1's first request carried the old field name.
        assert!(received[0].get("max_tokens").is_some());
        // Turn 1's retry already renamed it.
        assert!(received[1].get("max_completion_tokens").is_some());
        // Turn 2's single request was proactively renamed by apply_learned.
        assert!(
            received[2].get("max_completion_tokens").is_some(),
            "turn 2 body should already carry max_completion_tokens"
        );
        assert!(
            received[2].get("max_tokens").is_none(),
            "turn 2 body should NOT carry the old max_tokens key"
        );
    }

    /// External re-review P2: the 400-diagnose-resend loop must spend ONE
    /// absolute deadline for the whole chat() call. A mock that takes 700ms
    /// to answer the first request with a fixable 400 and 600ms to answer
    /// the adjusted request with success: with the budget set to 1s the
    /// resend has only ~300ms left and must time out — the pre-fix code
    /// handed every post a fresh `per_request_timeout` and reported success
    /// ~1.3s into a 1s budget.
    #[test]
    fn fix_resend_spends_the_same_budget_not_a_fresh_one() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let requests_c = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let body = read_http_body(&stream);
                // Count at receipt, not after the delayed response: the
                // timed-out resend aborts client-side while the mock is
                // still sleeping, and the assertion must not race it.
                requests_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let is_400 = body.contains("\"max_tokens\"");
                let (status, ct, payload, delay): (&str, &str, &str, u64) = if is_400 {
                    (
                        "400 Bad Request",
                        "application/json",
                        r#"{"error":{"message":"Unrecognized request argument: max_tokens","type":"invalid_request_error"}}"#,
                        700,
                    )
                } else {
                    (
                        "200 OK",
                        "text/event-stream",
                        concat!(
                            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n",
                            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                            "data: [DONE]\n\n",
                        ),
                        600,
                    )
                };
                std::thread::sleep(std::time::Duration::from_millis(delay));
                let head = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {ct}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    payload.len(),
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(payload.as_bytes());
                let _ = stream.flush();
            }
        });

        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        let provider = OpenAi::new(client, url, "k", std::time::Duration::from_secs(30));
        let m = vec![Message::user(vec![Block::Text("one".into())])];
        let req = ChatRequest {
            system: None,
            messages: &m,
            tools: &[],
            model: "m",
            max_tokens: 100,
            per_request_timeout: Some(std::time::Duration::from_secs(1)),
            cancel: Arc::new(AtomicBool::new(false)),
            cache: true,
            reasoning_effort: None,
            temperature: None,
        };
        let started = std::time::Instant::now();
        let r = provider.chat(&req, &mut |_| {});
        assert!(
            r.is_err(),
            "the resend must inherit the spent budget, not a fresh 1s window"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_millis(1250),
            "total wall clock must stay inside the 1s budget (+margin): {:?}",
            started.elapsed()
        );
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the 400 and the timed-out resend"
        );
    }

    /// The cancel checkpoint before the 400 resend: a cancel that arrived
    /// while the 400 round-trip was in flight aborts instead of starting
    /// another HTTP request (the run-level ladder treats the Err + flag as
    /// EXIT_CANCELLED). The flag is flipped SERVER-SIDE after the first
    /// request lands — the pre-POST checkpoint (review pin
    /// `openai_cancel_before_first_post_sends_nothing`) intercepts an
    /// already-set flag before the first request, so arming it before chat()
    /// would no longer reach the resend path.
    #[test]
    fn fix_resend_aborts_when_cancelled_during_the_400() {
        let (url, bodies, cancel) = max_tokens_400_mock_with_cancel_flip();
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        let provider = OpenAi::new(client, url, "k", std::time::Duration::from_secs(30));
        let m = vec![Message::user(vec![Block::Text("one".into())])];
        let req = ChatRequest {
            system: None,
            messages: &m,
            tools: &[],
            model: "m",
            max_tokens: 100,
            per_request_timeout: Some(std::time::Duration::from_secs(10)),
            cancel,
            cache: true,
            reasoning_effort: None,
            temperature: None,
        };
        let err = match provider.chat(&req, &mut |_| {}) {
            Err(e) => e,
            Ok(_) => panic!("chat must abort instead of resending after cancel"),
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cancelled while adjusting"),
            "error must name the cancel: {msg}"
        );
        assert_eq!(
            bodies.lock().unwrap().len(),
            1,
            "exactly the 400 request — no resend after cancel"
        );
    }

    /// A gateway that rejects the max_tokens VALUE (not the field name): both
    /// `max_tokens` and `max_completion_tokens` 400 with "must be ≤ 100". The
    /// rename fix fires on the first 400 (its text contains "max_tokens") but
    /// cannot succeed; the retry's 400 names `max_completion_tokens`, which no
    /// rule matches → bail. Under record-at-apply-time this failure STILL
    /// poisoned the learned-fix table, so every later turn proactively sent
    /// the renamed field. Record-on-success must learn nothing here.
    fn value_error_400_mock() -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let bodies_c = bodies.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let body = read_http_body(&stream);
                if let Ok(v) = serde_json::from_str::<Value>(&body) {
                    bodies_c.lock().unwrap().push(v);
                }
                // Order matters: check the RENAMED key first. "Invalid
                // max_completion_tokens: …" names the new field only, so the
                // retried request's 400 matches no diagnose_400 rule. BOTH
                // keys are value errors — the 400 fires either way.
                let (status, ct, payload): (&str, &str, &str) = if body
                    .contains("\"max_completion_tokens\"")
                {
                    (
                        "400 Bad Request",
                        "application/json",
                        r#"{"error":{"message":"Invalid max_completion_tokens: must be less than or equal to 100","type":"invalid_request_error"}}"#,
                    )
                } else if body.contains("\"max_tokens\"") {
                    (
                        "400 Bad Request",
                        "application/json",
                        r#"{"error":{"message":"Invalid max_tokens: must be less than or equal to 100","type":"invalid_request_error"}}"#,
                    )
                } else {
                    // Unreachable in this test (every request carries one
                    // of the two keys) — kept so a body-shape surprise
                    // still yields a parseable success instead of hanging.
                    (
                            "200 OK",
                            "text/event-stream",
                            concat!(
                                "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n",
                                "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                                "data: [DONE]\n\n",
                            ),
                        )
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {ct}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    payload.len(),
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(payload.as_bytes());
                let _ = stream.flush();
            }
        });
        (url, bodies)
    }

    #[test]
    fn value_error_400_does_not_poison_the_learned_fix() {
        let (url, bodies) = value_error_400_mock();
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        let provider = OpenAi::new(client, url, "k", std::time::Duration::from_secs(30));
        let cancel = Arc::new(AtomicBool::new(false));
        fn mk_req<'a>(m: &'a [Message], cancel: &Arc<AtomicBool>) -> ChatRequest<'a> {
            ChatRequest {
                system: None,
                messages: m,
                tools: &[],
                model: "m",
                max_tokens: 100,
                per_request_timeout: Some(std::time::Duration::from_secs(10)),
                cancel: cancel.clone(),
                cache: true,
                reasoning_effort: None,
                temperature: None,
            }
        }

        // Turn 1: 400 on max_tokens (value error) → rename fires → retry 400s
        // on max_completion_tokens → no rule → honest Err.
        let m1 = vec![Message::user(vec![Block::Text("turn one".into())])];
        let r1 = provider.chat(&mk_req(&m1, &cancel), &mut |_| {});
        assert!(r1.is_err(), "unfixable value 400 must fail the turn");

        // Turn 2: nothing was learned — the first request must STILL carry the
        // original `max_tokens` field (pre-fix it was proactively renamed).
        let m2 = vec![Message::user(vec![Block::Text("turn two".into())])];
        let _ = provider.chat(&mk_req(&m2, &cancel), &mut |_| {});
        let received = bodies.lock().unwrap().clone();
        // Turn 1: 400 + retry-400; turn 2: 400 + retry-400 (it fails the same
        // way — the point is it fails with the ORIGINAL body, not a poisoned
        // one).
        assert_eq!(received.len(), 4, "saw {} requests", received.len());
        assert!(received[0].get("max_tokens").is_some());
        assert!(received[1].get("max_completion_tokens").is_some());
        assert!(
            received[2].get("max_tokens").is_some(),
            "turn 2 must start from the un-poisoned body (no proactive rename)"
        );
    }

    /// The non-2xx error body is the gateway's choice; a malicious or broken
    /// one streaming an endless "error body" must not be buffered whole into
    /// the error message. 500 + 1 MiB body → the surfaced message stays
    /// bounded (< 32 KiB, i.e. the 16 KiB cap + the marker + message chrome)
    /// and carries the truncation marker, while the status still reads
    /// through the structured ApiStatusError.
    #[test]
    fn oversized_non_2xx_error_body_is_capped_in_the_error_message() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let _request = read_http_body(&stream);
                let payload = "E".repeat(1024 * 1024);
                let head = format!(
                    "HTTP/1.1 500 Internal Server Error\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    payload.len(),
                );
                // The client stops reading once its cap is hit and drops the
                // response, so the tail write may fail on a closed socket —
                // ignored, the client already has what it needs.
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(payload.as_bytes());
                let _ = stream.flush();
            }
        });
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        let provider = OpenAi::new(client, url, "k", std::time::Duration::from_secs(30));
        let m = vec![Message::user(vec![Block::Text("one".into())])];
        let req = ChatRequest {
            system: None,
            messages: &m,
            tools: &[],
            model: "m",
            max_tokens: 100,
            per_request_timeout: Some(std::time::Duration::from_secs(10)),
            cancel: Arc::new(AtomicBool::new(false)),
            cache: true,
            reasoning_effort: None,
            temperature: None,
        };
        let err = match provider.chat(&req, &mut |_| {}) {
            Err(e) => e,
            Ok(_) => panic!("a 500 must fail the request"),
        };
        // The structured status is unaffected by the cap.
        assert_eq!(
            err.downcast_ref::<crate::provider::ApiStatusError>()
                .expect("non-2xx surfaces as ApiStatusError")
                .status,
            500
        );
        let msg = format!("{err}");
        assert!(
            msg.len() < 32 * 1024,
            "error message must stay bounded, got {} bytes",
            msg.len()
        );
        assert!(msg.contains("500 Internal Server Error"), "{msg:.120}…");
        assert!(
            msg.contains("[truncated at"),
            "must mark the cut; tail: …{}",
            &msg[msg.len().saturating_sub(120)..]
        );
        // The cap kept the HEAD of the body (where diagnostics live).
        assert!(msg.contains("EEEEEE"), "{msg:.120}…");
    }

    /// FR-3 预置路径：上一进程学到的修复经 `Config::gateway_fixes_preset`
    /// → `with_preset` 回来后，本进程的**第一个请求**就带修复后的形状——
    /// 教学 400 及其重试往返整个消失（mock 只收到 1 个请求，且直接是改名
    /// 后的字段）。导出面 `gateway_fixes()` 如实回报预置位（导出/预置
    /// 往返等价）。
    #[test]
    fn preset_fixes_skip_the_teaching_400() {
        let (url, bodies) = max_tokens_400_mock();
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        let preset = crate::provider::GatewayFixes {
            max_completion_tokens: true,
            ..Default::default()
        };
        let provider =
            OpenAi::new(client, url, "k", std::time::Duration::from_secs(30)).with_preset(preset);
        let cancel = Arc::new(AtomicBool::new(false));
        let m1 = vec![Message::user(vec![Block::Text("turn one".into())])];
        let req1 = ChatRequest {
            system: None,
            messages: &m1,
            tools: &[],
            model: "m",
            max_tokens: 100,
            per_request_timeout: Some(std::time::Duration::from_secs(10)),
            cancel: cancel.clone(),
            cache: true,
            reasoning_effort: None,
            temperature: None,
        };
        let r1 = provider
            .chat(&req1, &mut |_| {})
            .expect("first request succeeds on the preset");
        assert_eq!(r1.stop_reason, "end_turn");
        let received = bodies.lock().unwrap().clone();
        assert_eq!(
            received.len(),
            1,
            "preset must skip the teaching 400 entirely; saw {} requests",
            received.len()
        );
        assert!(received[0].get("max_completion_tokens").is_some());
        assert!(received[0].get("max_tokens").is_none());
        // 导出/预置往返等价。
        assert_eq!(provider.gateway_fixes(), preset);
    }

    /// FR-3 导出路径：一个真实 400 教出来的修复，经 `gateway_fixes()` 可
    /// 被宿主取走（放进下一进程的 Config 预置）。位与位分离：只有真的
    /// 学到的那一位为 true。
    #[test]
    fn learned_fixes_export_through_gateway_fixes() {
        let (url, _bodies) = max_tokens_400_mock();
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        let provider = OpenAi::new(client, url, "k", std::time::Duration::from_secs(30));
        assert_eq!(
            provider.gateway_fixes(),
            crate::provider::GatewayFixes::default()
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let m1 = vec![Message::user(vec![Block::Text("turn one".into())])];
        let req1 = ChatRequest {
            system: None,
            messages: &m1,
            tools: &[],
            model: "m",
            max_tokens: 100,
            per_request_timeout: Some(std::time::Duration::from_secs(10)),
            cancel,
            cache: true,
            reasoning_effort: None,
            temperature: None,
        };
        provider
            .chat(&req1, &mut |_| {})
            .expect("run recovers after the teaching 400");
        assert_eq!(
            provider.gateway_fixes(),
            crate::provider::GatewayFixes {
                no_reasoning_echo: false,
                max_completion_tokens: true,
                no_stream_options: false,
                no_reasoning_effort: false,
            },
            "exactly the learned bit exports"
        );
    }

    /// Loopback gateway that takes no `reasoning_content` on assistant
    /// messages: any request whose body carries the key gets a 400 naming it
    /// (driving diagnose_400 → DropReasoningEcho); every other request gets a
    /// minimal success SSE. Bodies recorded in arrival order.
    fn reasoning_echo_400_mock() -> (String, Arc<Mutex<Vec<Value>>>) {
        crate::provider::testhttp::gateway(|body| {
            let echoes = serde_json::from_str::<Value>(body)
                .map(|v| body_echoes_reasoning(&v))
                .unwrap_or(false);
            if echoes {
                crate::provider::testhttp::json_error(
                    "400 Bad Request",
                    "Invalid value for 'messages[1]': reasoning_content is not supported",
                )
            } else {
                crate::provider::testhttp::sse_says("hi")
            }
        })
    }

    /// A request whose history echoes a reasoning trace on the assistant turn
    /// (what the agent sends inside a tool loop while reasoning carry is on).
    fn echoing_req(cancel: &Arc<AtomicBool>) -> ChatRequest<'static> {
        let msgs: &'static [Message] = Box::leak(
            vec![
                Message::user(vec![Block::Text("turn one".into())]),
                carried("openai", &["thinking"]),
            ]
            .into_boxed_slice(),
        );
        ChatRequest {
            system: None,
            messages: msgs,
            tools: &[],
            model: "m",
            max_tokens: 100,
            per_request_timeout: Some(std::time::Duration::from_secs(10)),
            cancel: cancel.clone(),
            cache: true,
            reasoning_effort: None,
            temperature: None,
        }
    }

    /// The echo's own teaching 400, end to end: the first request carries
    /// `reasoning_content`, the gateway rejects the field, the resend strips
    /// it and succeeds, and the fix is recorded so the NEXT turn's first
    /// request already goes out without it (no repeated 400 round-trip).
    #[test]
    fn reasoning_echo_400_teaches_the_provider_to_stop_echoing() {
        let (url, bodies) = reasoning_echo_400_mock();
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        let provider = OpenAi::new(client, url, "k", std::time::Duration::from_secs(30));
        let cancel = Arc::new(AtomicBool::new(false));
        let req = echoing_req(&cancel);

        let r1 = provider
            .chat(&req, &mut |_| {})
            .expect("turn 1 recovers after the teaching 400");
        assert_eq!(r1.stop_reason, "end_turn");
        let r2 = provider.chat(&req, &mut |_| {}).expect("turn 2 succeeds");
        assert_eq!(r2.stop_reason, "end_turn");

        let received = bodies.lock().unwrap().clone();
        assert_eq!(
            received.len(),
            3,
            "turn 1 = 400 + fixed resend, turn 2 = one request; saw {}",
            received.len()
        );
        assert_eq!(received[0]["messages"][1]["reasoning_content"], "thinking");
        assert!(
            received[1]["messages"][1]
                .get("reasoning_content")
                .is_none(),
            "the resend must carry the stripped body"
        );
        assert!(
            received[2]["messages"][1]
                .get("reasoning_content")
                .is_none(),
            "turn 2 must go out without the echo from the start"
        );
        assert_eq!(
            provider.gateway_fixes(),
            crate::provider::GatewayFixes {
                no_reasoning_echo: true,
                ..Default::default()
            },
            "exactly the learned bit exports"
        );
    }

    /// A preset `no_reasoning_echo` (learned in a previous process, fed
    /// through `Config::gateway_fixes_preset`) makes the very first request
    /// skip the echo — and the teaching 400 with it.
    #[test]
    fn preset_no_reasoning_echo_skips_the_teaching_400() {
        let (url, bodies) = reasoning_echo_400_mock();
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        let preset = crate::provider::GatewayFixes {
            no_reasoning_echo: true,
            ..Default::default()
        };
        let provider =
            OpenAi::new(client, url, "k", std::time::Duration::from_secs(30)).with_preset(preset);
        let cancel = Arc::new(AtomicBool::new(false));
        let r = provider
            .chat(&echoing_req(&cancel), &mut |_| {})
            .expect("request succeeds on the preset");
        assert_eq!(r.stop_reason, "end_turn");
        let received = bodies.lock().unwrap().clone();
        assert_eq!(
            received.len(),
            1,
            "preset must skip the teaching 400 entirely; saw {} requests",
            received.len()
        );
        assert!(received[0]["messages"][1]
            .get("reasoning_content")
            .is_none());
        assert_eq!(received[0]["messages"][1]["content"], "answer");
        // 导出/预置往返等价。
        assert_eq!(provider.gateway_fixes(), preset);
    }

    /// The other reasoning 400: carry is off (the default), the request
    /// carries no `reasoning_content`, and the endpoint insists on it. No
    /// body edit can supply what was never kept, so the turn fails honestly —
    /// with the text naming the switch that repairs it.
    #[test]
    fn missing_reasoning_400_names_the_carry_switch() {
        let (url, bodies) = crate::provider::testhttp::gateway(|_| {
            crate::provider::testhttp::json_error(
                "400 Bad Request",
                "Missing `reasoning_content` field in the assistant message at message index 1",
            )
        });
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        let provider = OpenAi::new(client, url, "k", std::time::Duration::from_secs(30));
        let cancel = Arc::new(AtomicBool::new(false));
        let m = vec![
            Message::user(vec![Block::Text("q".into())]),
            Message::assistant(vec![Block::Text("a".into())]),
        ];
        let req = ChatRequest {
            system: None,
            messages: &m,
            tools: &[],
            model: "m",
            max_tokens: 100,
            per_request_timeout: Some(std::time::Duration::from_secs(10)),
            cancel,
            cache: true,
            reasoning_effort: None,
            temperature: None,
        };
        let err = provider
            .chat(&req, &mut |_| {})
            .err()
            .expect("nothing to resend — the missing trace was never kept")
            .to_string();
        assert!(err.contains("Missing `reasoning_content`"), "{err}");
        assert!(
            err.contains("`preserved_reasoning: true`"),
            "the 400 must name the switch: {err}"
        );
        // One request, nothing learned: there is no field to drop.
        assert_eq!(bodies.lock().unwrap().len(), 1);
        assert_eq!(
            provider.gateway_fixes(),
            crate::provider::GatewayFixes::default()
        );
    }

    /// The hint is specific to the reasoning 400 — any other 400 keeps its
    /// plain text.
    #[test]
    fn unrelated_400_carries_no_carry_hint() {
        let (url, _bodies) = crate::provider::testhttp::gateway(|_| {
            crate::provider::testhttp::json_error(
                "400 Bad Request",
                "your account is out of credits",
            )
        });
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        let provider = OpenAi::new(client, url, "k", std::time::Duration::from_secs(30));
        let cancel = Arc::new(AtomicBool::new(false));
        let m = vec![Message::user(vec![Block::Text("q".into())])];
        let req = ChatRequest {
            system: None,
            messages: &m,
            tools: &[],
            model: "m",
            max_tokens: 100,
            per_request_timeout: Some(std::time::Duration::from_secs(10)),
            cancel,
            cache: true,
            reasoning_effort: None,
            temperature: None,
        };
        let err = provider
            .chat(&req, &mut |_| {})
            .err()
            .expect("an unfixable 400 fails the turn")
            .to_string();
        assert!(err.contains("out of credits"), "{err}");
        assert!(!err.contains("preserved_reasoning"), "{err}");
    }

    // ── FR-2 缩减版：内联 <think> 剥离（ThinkStripper） ──────────────────

    fn run_stripper(chunks: &[&str]) -> (String, String) {
        let mut st = ThinkStripper::default();
        let (mut text, mut think) = (String::new(), String::new());
        for c in chunks {
            for (is_text, piece) in st.feed(c) {
                if is_text {
                    text.push_str(&piece);
                } else {
                    think.push_str(&piece);
                }
            }
        }
        for (is_text, piece) in st.finish() {
            if is_text {
                text.push_str(&piece);
            } else {
                think.push_str(&piece);
            }
        }
        (text, think)
    }

    #[test]
    fn think_stripper_plain_content_passes_byte_identical() {
        // 无标签正文：逐字节透传（含尖括号），thinking 通道恒空，finish 无残。
        let (text, think) = run_stripper(&["hello < world", " & ", "ok"]);
        assert_eq!(text, "hello < world & ok");
        assert_eq!(think, "");
    }

    #[test]
    fn think_stripper_splits_tags_across_chunk_boundaries() {
        // 标签被任意切开也能认出；text 无 "<think"/"</think" 残留，字节不丢。
        let chunks = ["<th", "ink>let me ", "check</thi", "nk>the answer is 4"];
        let (text, think) = run_stripper(&chunks);
        assert_eq!(text, "the answer is 4");
        assert_eq!(think, "let me check");
        assert!(!text.contains("think"), "no tag residue: {text}");
    }

    #[test]
    fn think_stripper_handles_multiple_blocks_and_mixed_text() {
        let (text, think) = run_stripper(&["pre<think>a</think>mid<think>b</think>post"]);
        assert_eq!(text, "premidpost");
        assert_eq!(think, "ab");
    }

    #[test]
    fn think_stripper_unclosed_block_stays_thinking_at_eof() {
        // 完整开标签后流断了：余量按 thinking 冲（正文纯净 fail-closed）。
        let (text, think) = run_stripper(&["<think>partial thought"]);
        assert_eq!(text, "");
        assert_eq!(think, "partial thought");
    }

    #[test]
    fn think_stripper_unterminated_partial_tag_is_text_at_eof() {
        // 没成形的 "<thin" 从来不是标签：finish 按正文冲。
        let (text, think) = run_stripper(&["answer <thin"]);
        assert_eq!(text, "answer <thin");
        assert_eq!(think, "");
    }

    #[test]
    fn think_stripper_close_without_open_is_text() {
        let (text, think) = run_stripper(&["a </think> b"]);
        assert_eq!(text, "a </think> b");
        assert_eq!(think, "");
    }

    #[test]
    fn think_stripper_splits_evenly_at_every_offset() {
        // 穷举切片点：任意 3 字节切片下输出与整段处理一致（跨边界不变式）。
        let whole = "q<think>reason part</think>a</t";
        for step in 1..=5 {
            let chunks: Vec<&str> = whole
                .as_bytes()
                .chunks(step)
                .map(|b| std::str::from_utf8(b).unwrap())
                .collect();
            let (text, think) = run_stripper(&chunks);
            assert_eq!(text, "qa</t", "step={step}");
            assert_eq!(think, "reason part", "step={step}");
        }
    }

    #[test]
    fn parse_stream_routes_inline_think_to_thinking_events() {
        // SSE 集成（knob 默认开）：content 里的 <think> 块 → ThinkingDelta
        // 事件拼出推理；blocks 的 Text 无标签残留（不入历史的内容面）。
        // reasoning_content 形态照旧（协议字段，不受此开关管）。
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"<think>reason a\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"</think>the answer\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
        );
        let mut events = Vec::new();
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |e| events.push(e)).unwrap();
        assert_eq!(resp.stop_reason, "end_turn");
        assert!(
            matches!(&resp.blocks[0], Block::Text(t) if t == "the answer"),
            "text block has no tag residue: {:?}",
            resp.blocks
        );
        let thinking: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::ThinkingDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, "reason a");
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "the answer");
    }

    #[test]
    fn parse_loop_with_knob_off_passes_tags_through() {
        // 开关关（Config::strip_think_tags=false）：标签当普通正文透传，
        // 与既有（FR-2 之前）行为字节一致。
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"<think>kept\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
        );
        let mut sse = SseReader::new(raw.as_bytes());
        let mut events = Vec::new();
        let resp = OpenAi::parse_loop(&mut sse, &mut |e| events.push(e), false, None).unwrap();
        assert!(matches!(&resp.blocks[0], Block::Text(t) if t == "<think>kept"));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ChatEvent::ThinkingDelta(_))),
            "knob off: no thinking channel traffic"
        );
    }

    #[test]
    fn parse_stream_mixed_reasoning_field_and_inline_think_do_not_cross() {
        // FR-2 验收③（外审 P2-6 补钉）：同一流里 reasoning_content 协议字段
        // 与内联 <think> 内容约定两形态混跑——各自进 ThinkingDelta、互不串
        // 线，正文无标签残留。
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"proto-think \"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"<think>inline-think </think>\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"still proto\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"the answer\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
        );
        let mut events = Vec::new();
        let resp = OpenAi::parse_stream(raw.as_bytes(), &mut |e| events.push(e)).unwrap();
        assert_eq!(resp.stop_reason, "end_turn");
        assert!(
            matches!(&resp.blocks[0], Block::Text(t) if t == "the answer"),
            "text block clean: {:?}",
            resp.blocks
        );
        let thinking: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::ThinkingDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        // 协议字段原样、内联标签剥离——两条通道的量都到齐、无标签残留。
        assert_eq!(thinking, "proto-think inline-think still proto");
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "the answer");
        assert!(
            !text.contains("think"),
            "no tag residue in the reply channel"
        );
    }

    #[test]
    fn knob_setter_copies_the_config_value_verbatim() {
        // 外审返工补钉：`with_strip_think_tags` 是 FR-2 的唯一生产布线点
        // （provider::build）的末端——取反/接反会让默认行为整体翻转而单元
        // 测试全绿（它们直接给 parse_loop 传参，绕过 setter）。preset 面
        // 的 build 布线由 provider::tests::build_wires_the_config_knobs 钉。
        let o = OpenAi::new(
            reqwest::blocking::Client::new(),
            "http://127.0.0.1:9/v1".into(),
            "k",
            std::time::Duration::from_secs(30),
        );
        assert!(o.strip_think_tags, "default is on");
        let o = o.with_strip_think_tags(false);
        assert!(!o.strip_think_tags);
        let o = o.with_strip_think_tags(true);
        assert!(o.strip_think_tags);
    }

    /// A plain one-message request (no carried reasoning, no tools).
    fn plain_req() -> ChatRequest<'static> {
        let msgs: &'static [Message] =
            Box::leak(vec![Message::user(vec![Block::Text("hi".into())])].into_boxed_slice());
        ChatRequest {
            system: None,
            messages: msgs,
            tools: &[],
            model: "m",
            max_tokens: 100,
            per_request_timeout: Some(std::time::Duration::from_secs(10)),
            cancel: Arc::new(AtomicBool::new(false)),
            cache: true,
            reasoning_effort: None,
            temperature: None,
        }
    }

    /// `Config.extra_body` / `extra_headers` reach the wire through the one
    /// production wiring point (`provider::build`): new fields and headers
    /// ride along, while a kernel-written body field or header keeps its
    /// value — here from a config that skipped the load-time check (built
    /// straight from JSON, reserved entries and all).
    #[test]
    fn chat_sends_the_config_extras_on_the_wire() {
        use crate::provider::testhttp;
        let gw = testhttp::gateway_inspected(|_| testhttp::sse_says("ok"));
        let cfg: crate::config::Config = serde_json::from_str(&format!(
            r#"{{"provider":"openai","api_key_env":"K","model":"m","base_url":{:?},
                "extra_body":{{"top_p":0.9,"model":"smuggled","stream":false}},
                "extra_headers":{{"X-Title":"app","Authorization":"Bearer evil",
                                  "HTTP-Referer":"https://example.test"}}}}"#,
            gw.url
        ))
        .unwrap();
        let prov = crate::provider::build(&cfg, "k", reqwest::blocking::Client::new());
        let req = plain_req();
        let resp = prov.chat(&req, &mut |_| {}).unwrap();
        assert!(
            matches!(resp.blocks.as_slice(), [Block::Text(t)] if t == "ok"),
            "{:?}",
            resp.blocks
        );
        let bodies = gw.bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["top_p"], 0.9);
        assert_eq!(bodies[0]["model"], "m");
        assert_eq!(bodies[0]["max_tokens"], 100);
        assert_eq!(
            bodies[0]["stream"], true,
            "streaming is the kernel's decision"
        );
        let heads = gw.heads.lock().unwrap();
        let head = &heads[0];
        for wanted in [
            "authorization: bearer k",
            "content-type: application/json",
            "x-title: app",
            "http-referer: https://example.test",
        ] {
            assert!(
                head.iter().any(|l| l == wanted),
                "{wanted} missing from {head:?}"
            );
        }
        assert!(
            !head
                .iter()
                .any(|l| l.starts_with("authorization: bearer evil")),
            "the key header cannot be swapped from the extras: {head:?}"
        );
    }

    /// A fix learned from a 400 about a field only the extras supply must
    /// stick: the extras are merged BEFORE the learned fixes, so the next
    /// turn's first request already goes out without the field instead of
    /// paying the teaching 400 again on every turn.
    #[test]
    fn learned_fix_outlasts_a_field_supplied_by_extra_body() {
        use crate::provider::testhttp;
        let gw = testhttp::gateway_inspected(|body| {
            if body.contains("reasoning_effort") {
                testhttp::json_error(
                    "400 Bad Request",
                    "Unrecognized request argument supplied: reasoning_effort",
                )
            } else {
                testhttp::sse_says("ok")
            }
        });
        let cfg: crate::config::Config = serde_json::from_str(&format!(
            r#"{{"provider":"openai","api_key_env":"K","model":"m","base_url":{:?},
                "extra_body":{{"reasoning_effort":"high"}}}}"#,
            gw.url
        ))
        .unwrap();
        let prov = crate::provider::build(&cfg, "k", reqwest::blocking::Client::new());
        let req = plain_req();
        prov.chat(&req, &mut |_| {}).unwrap();
        prov.chat(&req, &mut |_| {}).unwrap();
        let heads = gw.heads.lock().unwrap();
        assert_eq!(
            heads.len(),
            3,
            "teaching 400 + its resend, then one request"
        );
        let bodies = gw.bodies.lock().unwrap();
        assert_eq!(
            bodies[0]["reasoning_effort"], "high",
            "the extra rides along"
        );
        assert!(
            bodies[1].get("reasoning_effort").is_none(),
            "fixed on the resend"
        );
        assert!(
            bodies[2].get("reasoning_effort").is_none(),
            "the learned fix outlasts the extras merge"
        );
    }
}
