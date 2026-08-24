mod anthropic;
mod openai;
// The scripted mock provider is test-only by default; the `mock` cargo feature
// (used by the Python / TypeScript bindings and downstream dry-runs) exposes it
// so an agent loop can be driven offline, with no API key or HTTP server.
#[cfg(any(test, feature = "mock"))]
pub mod mock;

pub use anthropic::Anthropic;
pub use openai::OpenAi;

use crate::config::{Config, ProviderKind};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

/// One block of message content. Mirrors Anthropic's content-block model;
/// the OpenAI provider maps these onto its own wire format.
///
/// `#[non_exhaustive]`: new content block types may be added; external
/// `match`es must keep a wildcard arm (serde (de)serialization is unaffected —
/// it goes by tag, not by exhaustive matching). See `AgentEvent`'s doc for the
/// consumer-side-only hygiene rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Block {
    Text(String),
    /// base64-encoded image (media_type e.g. "image/png").
    Image { media_type: String, data: String },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        /// Optional image payload attached to the result (e.g. load_image).
        image: Option<Box<Block>>,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Block>,
}

impl Message {
    pub fn user(blocks: Vec<Block>) -> Self {
        Self {
            role: Role::User,
            content: blocks,
        }
    }

    pub fn assistant(blocks: Vec<Block>) -> Self {
        Self {
            role: Role::Assistant,
            content: blocks,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input object.
    pub schema: Value,
}

/// Incremental events emitted while streaming a response.
///
/// `#[non_exhaustive]`: new stream event kinds may be added; external
/// `match`es must keep a wildcard arm. See `AgentEvent`'s doc for the
/// consumer-side-only hygiene rule.
#[non_exhaustive]
pub enum ChatEvent {
    TextDelta(String),
    /// Reasoning/thinking increment from a reasoning model (OpenAI-style
    /// `reasoning_content`, Anthropic `thinking` blocks). Surfaced as an event
    /// ONLY — it never enters the conversation history (`text`/`blocks`), since
    /// no major API requires thinking to be echoed back on the next turn
    /// (DeepSeek V4 thinking + tool-use multi-turn is the documented exception;
    /// not handled here).
    ThinkingDelta(String),
}

/// Token usage for one chat. Best-effort: None when the server didn't report
/// (request errored, or the stream was cut off before the usage frame).
///
/// `cache_creation_input_tokens` / `cache_read_input_tokens` cover prompt
/// caching: Anthropic reports both (writes billed 1.25×, reads 0.1×); OpenAI
/// reports only cached reads via `prompt_tokens_details.cached_tokens`. Both
/// default to 0 when the provider doesn't report caching. `input_tokens` is the
/// *non-cached* input (the two cache fields are separate), so the honest billed
/// input is `input_tokens + cache_creation*1.25 + cache_read*0.1`.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    /// Defaults to 0 when a custom provider omits it (`#[serde(default)]` — an
    /// unreported axis reads as "no tokens here", matching how the built-in
    /// providers treat axes the server didn't return).
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl Usage {
    /// Sum of all token axes — input + output + cache write + cache read.
    /// The run's `max_total_tokens` budget guard checks this against the cap at
    /// each turn boundary. Includes cache_read even though providers discount
    /// it: it still reflects tokens transmitted, and the guard is a coarse
    /// "stop the runaway" backstop, not a billing-precise meter.
    pub fn total(&self) -> u64 {
        // saturating_add so a garbage u64 axis (e.g. a buggy/malicious
        // provider feeding huge values) can't wrap the sum past the
        // max_total_tokens budget guard — it saturates at u64::MAX instead.
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
    }
}

/// A completed assistant turn.
pub struct ChatResponse {
    pub blocks: Vec<Block>,
    /// "end_turn" | "tool_use" | "max_tokens" | provider-specific value.
    pub stop_reason: String,
    pub usage: Option<Usage>,
}

/// A simplified, **non-streaming** chat response shape: the full assistant text
/// plus optional `stop_reason` and `usage`. This is the contract the Python /
/// TypeScript bindings ask a user-supplied custom provider to return, and a
/// convenience for Rust callers who don't want to assemble [`Block`]s by hand.
///
/// A custom provider returns the whole text at once (no incremental deltas);
/// [`ChatResponse::from_simple`] emits that text as a single [`ChatEvent::
/// TextDelta`] so the agent loop — which drives display via `on_event` — still
/// sees the assistant message exactly once. Real streaming is the built-in
/// providers' job; a non-streaming custom provider still drives the loop
/// correctly for mock / adapter / non-streaming-LLM use cases.
///
/// `text` is the only required field. Omitting `stop_reason` defaults to
/// `"end_turn"`; omitting `usage` reports no token cost for the turn. This
/// shape is text-only — a custom provider cannot emit `tool_use` blocks via
/// this path (use the built-in provider + custom [`crate::tools::Tool`]s for
/// tool use).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SimpleChatResponse {
    /// The full assistant text for this turn.
    pub text: String,
    /// Defaults to `"end_turn"` when omitted or `None`.
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// Token usage for this turn; `None` means "unreported" (the turn is free).
    #[serde(default)]
    pub usage: Option<Usage>,
}

impl SimpleChatResponse {
    /// Parse from an arbitrary JSON [`Value`] (e.g. what a binding received from
    /// the host language). Missing `stop_reason`/`usage` default per the struct's
    /// `#[serde(default)]` rules.
    pub fn from_value(v: &Value) -> Result<Self> {
        serde_json::from_value(v.clone()).map_err(|e| anyhow::anyhow!("custom provider response: {e}"))
    }
}

impl ChatResponse {
    /// Assemble a [`ChatResponse`] from a [`SimpleChatResponse`], emitting the
    /// full text as a single [`ChatEvent::TextDelta`] via `on_event` (so the
    /// agent's streaming display / event forwarding sees it). Empty text yields
    /// **no blocks** — matching the agent loop's "don't persist an empty
    /// assistant turn" invariant (an empty assistant message makes strict APIs
    /// reject the next request); the loop breaks without saving it. Non-empty
    /// text produces exactly one [`Block::Text`].
    pub fn from_simple(s: SimpleChatResponse, on_event: &mut dyn FnMut(ChatEvent)) -> ChatResponse {
        let blocks = if s.text.is_empty() {
            Vec::new()
        } else {
            on_event(ChatEvent::TextDelta(s.text.clone()));
            vec![Block::Text(s.text)]
        };
        ChatResponse {
            blocks,
            stop_reason: s.stop_reason.unwrap_or_else(|| "end_turn".to_string()),
            usage: s.usage,
        }
    }
}

pub struct ChatRequest<'a> {
    pub system: Option<&'a str>,
    pub messages: &'a [Message],
    pub tools: &'a [ToolDef],
    pub model: &'a str,
    pub max_tokens: u32,
    /// Per-request timeout that overrides the client default, used to keep
    /// a single chat inside the run's remaining budget (None = use the
    /// client's configured timeout). The run budget is otherwise only
    /// checked at turn boundaries, so a stuck request could outlive it.
    pub per_request_timeout: Option<std::time::Duration>,
    /// Cancellation flag shared with the agent run. The streaming reader
    /// checks it between chunks so Esc/Ctrl-C can interrupt a stuck request
    /// mid-stream, not only at the next turn boundary.
    pub cancel: Arc<AtomicBool>,
    /// When true (the normal agent turn), the Anthropic provider marks prompt-
    /// caching breakpoints. One-off requests that won't be re-sent with a
    /// growing prefix (e.g. the compression summary) pass false to skip the
    /// 1.25× cache-write premium on content that will never be a cache read.
    /// OpenAI caching is implicit and ignores this.
    pub cache: bool,
    /// Reasoning effort forwarded to the provider verbatim (no enum validation
    /// — vendor ladders differ and shift across generations); `None` = server
    /// default. One-off requests (the compression summary) pass `None`.
    pub reasoning_effort: Option<&'a str>,
}

pub trait Provider: Send + Sync {
    /// Send one chat request, streaming text deltas to `on_event`.
    /// Returns the fully-assembled assistant message.
    fn chat(&self, req: &ChatRequest, on_event: &mut dyn FnMut(ChatEvent)) -> Result<ChatResponse>;
}

pub fn build(cfg: &Config, api_key: &str, client: reqwest::blocking::Client) -> Box<dyn Provider> {
    let base = cfg.base_url();
    // Clamp ≥1: 0 would be an instant always-firing idle timeout. The 30s
    // default suits ordinary chat models (sub-second inter-chunk gaps);
    // deep-reasoning models with long first-token latency (DeepSeek-R1,
    // o-series) raise it via `limits.stream_idle_secs` in the config.
    let idle = Duration::from_secs(cfg.limits.stream_idle_secs.max(1));
    match cfg.provider {
        ProviderKind::Anthropic => Box::new(Anthropic::new(client, base, api_key, idle)),
        ProviderKind::Openai => Box::new(OpenAi::new(client, base, api_key, idle)),
    }
}

/// HTTP client with explicit timeouts. A stuck or trickling connection
/// must never outlive the whole run budget — an agent that hangs forever
/// would stall any CI/eval pipeline it runs in. Clones share one
/// connection pool (cheap, Arc inside).
///
/// `run_timeout_secs == 0` means "no timeout" (mirroring the agent loop):
/// no client-level TOTAL timeout is set — a hard 60s floor here would
/// silently kill any streaming generation longer than a minute even though
/// the user asked for no limit. The reader-thread leak backstop is handled
/// separately, as a per-request timeout set on each streaming request in
/// `agent::run_impl` (see `STREAM_BACKSTOP`) — reqwest's blocking client
/// exposes no idle-read timeout, so a per-request total cap is the only
/// available way to guarantee the `sse-reader` worker thread terminates on
/// a half-open connection. See `CancellableSse`.
pub fn new_client(run_timeout_secs: u64) -> reqwest::blocking::Client {
    let mut builder = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        // Don't follow redirects: a custom base_url that redirects could leak
        // the API key header (x-api-key / Authorization) to a third party.
        .redirect(reqwest::redirect::Policy::none());
    if run_timeout_secs > 0 {
        builder = builder.timeout(std::time::Duration::from_secs(run_timeout_secs.max(60)));
    }
    // Build failure is near-impossible (rustls init), but the fallback must
    // keep the no-redirect policy — a default Client follows redirects and
    // would reintroduce the key-leak vector above.
    builder.build().unwrap_or_else(|_| {
        reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client build")
    })
}

/// Iterate over the `data:` payloads of an SSE stream.
/// Handles lines split across read boundaries and ignores comments/keep-alives.
pub(crate) struct SseReader<R: Read> {
    reader: BufReader<R>,
}

impl<R: Read> SseReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            reader: BufReader::new(inner),
        }
    }

    /// Returns the next `data:` payload, or `Ok(None)` at clean end of stream.
    /// A read error surfaces as `Err` (NOT `Ok(None)`) so a mid-stream failure
    /// — connection reset, truncated body — isn't indistinguishable from a
    /// clean EOF (which would make the caller assemble a truncated "successful"
    /// response). The `SseSource` impl maps this to `StreamAbort::StreamError`.
    pub fn next_data(&mut self) -> std::io::Result<Option<String>> {
        let mut line = String::new();
        loop {
            line.clear();
            match self.reader.read_line(&mut line) {
                Ok(0) => return Ok(None),
                Ok(_) => {
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    if let Some(data) = trimmed.strip_prefix("data:") {
                        return Ok(Some(data.trim_start().to_string()));
                    }
                    // skip event:, id:, comments and blank lines
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// Why a cancellable stream read stopped early. `SseReader` (sync) only
/// produces `StreamError`; `CancellableSse` adds cancel/idle.
#[derive(Debug, Clone)]
pub(crate) enum StreamAbort {
    /// Caller flipped the shared cancel flag (Esc/Ctrl-C in the TUI).
    Cancelled,
    /// No new chunk for `idle` (the configured `limits.stream_idle_secs`) — the
/// connection is half-open
    /// (server stopped sending without closing). Distinct from the reqwest
    /// total timeout, which is a backstop for the whole request.
    IdleTimeout,
    /// The underlying read failed (connection reset, truncated body, …). This
    /// must NOT be folded into a clean EOF: a mid-stream IO error leaves the
    /// response partial, so the caller bails (→ transient retry) instead of
    /// assembling a truncated response and calling it success.
    StreamError(String),
}

/// Source of `data:` payloads. `SseReader` reads synchronously (tests);
/// `CancellableSse` drives a reader thread with an idle timeout + cancel flag
/// so a stuck or cancelled stream can be interrupted mid-read.
pub(crate) trait SseSource {
    /// Next payload, `Ok(None)` at end of stream, `Err` if cancelled/idle.
    fn next_payload(&mut self) -> std::result::Result<Option<String>, StreamAbort>;
}

impl<R: Read> SseSource for SseReader<R> {
    fn next_payload(&mut self) -> std::result::Result<Option<String>, StreamAbort> {
        match self.next_data() {
            Ok(opt) => Ok(opt),
            Err(e) => Err(StreamAbort::StreamError(e.to_string())),
        }
    }
}

/// Cancel/idle poll granularity. Bounds how long after Esc the stream read
/// actually returns — small enough to feel instant, large enough to avoid
/// needless wakeups on a healthy stream. (The idle threshold itself comes from
/// `CancellableSse::idle`, set from `limits.stream_idle_secs`.)
const POLL_SLICE: Duration = Duration::from_millis(200);

/// Wraps a blocking reader in a worker thread, exposing payloads with an idle
/// timeout and cancel check. The reader thread can't observe cancel (it's
/// blocked in `read`), so on cancel/timeout the caller stops polling. The
/// thread then exits as soon as its blocked read returns — which is guaranteed
/// because every streaming request carries a per-request timeout (from the
/// run budget, or the `STREAM_BACKSTOP` in `agent` when there is no budget):
/// that cap aborts the underlying read, so the thread can never linger
/// indefinitely on a half-open connection. reqwest's blocking client has no
/// idle-read timeout, so this per-request cap is the only available backstop.
/// Bounded, no busy-wait.
pub(crate) struct CancellableSse {
    rx: mpsc::Receiver<std::result::Result<String, String>>,
    cancel: Arc<AtomicBool>,
    /// Max gap between payloads before the stream is treated as stalled.
    /// Pluggable (from `limits.stream_idle_secs`) so deep-reasoning models
    /// with long first-token latency aren't wrongly aborted.
    idle: Duration,
}

impl CancellableSse {
    /// Spawn the reader thread. Fails (rather than panics) when the OS can't
    /// spawn a thread — thread-resource exhaustion is a per-request failure,
    /// handled by the caller like any transient request error (retry / cancel
    /// path), not a process-killing panic.
    pub(crate) fn new<R: Read + Send + 'static>(
        reader: R,
        cancel: Arc<AtomicBool>,
        idle: Duration,
    ) -> std::result::Result<Self, anyhow::Error> {
        // The channel carries Result: Ok(payload) for data, Err(message) when
        // the underlying read failed. An IO error is sent before the worker
        // drops the sender, so the receiver distinguishes a failed read from a
        // clean stream end (Disconnected) instead of masking both as EOF.
        let (tx, rx) = mpsc::channel::<std::result::Result<String, String>>();
        thread::Builder::new()
            .name("sse-reader".into())
            .spawn(move || {
                let mut sse = SseReader::new(reader);
                loop {
                    match sse.next_data() {
                        Ok(Some(d)) => {
                            if tx.send(Ok(d)).is_err() {
                                break; // caller dropped the receiver → stop pumping
                            }
                        }
                        Ok(None) => break, // clean EOF → drop tx → Disconnected
                        Err(e) => {
                            // Surface the read error before disconnecting.
                            let _ = tx.send(Err(e.to_string()));
                            break;
                        }
                    }
                }
                // tx drops here → receiver sees Disconnected (end of stream)
                // unless it already saw the Err above.
            })
            .context("spawn sse-reader thread")?;
        Ok(Self { rx, cancel, idle })
    }
}

impl SseSource for CancellableSse {
    fn next_payload(&mut self) -> std::result::Result<Option<String>, StreamAbort> {
        // Poll in short slices so a cancel flip (Esc/Ctrl-C) is noticed within
        // ~POLL_SLICE, not only when the idle timeout eventually fires. A
        // stuck connection still hits IdleTimeout after `self.idle`.
        let start = Instant::now();
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(StreamAbort::Cancelled);
            }
            if start.elapsed() >= self.idle {
                return Err(StreamAbort::IdleTimeout);
            }
            match self.rx.recv_timeout(POLL_SLICE) {
                Ok(Ok(d)) => return Ok(Some(d)),
                Ok(Err(msg)) => return Err(StreamAbort::StreamError(msg)),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(None), // reader ended
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_reader_skips_non_data_lines() {
        let raw = b"event: message_start\ndata: {\"a\":1}\n\ndata:{\"b\":2}\n: keep-alive\ndata: [DONE]\n";
        let mut r = SseReader::new(&raw[..]);
        assert_eq!(r.next_data().unwrap().as_deref(), Some("{\"a\":1}"));
        assert_eq!(r.next_data().unwrap().as_deref(), Some("{\"b\":2}"));
        assert_eq!(r.next_data().unwrap().as_deref(), Some("[DONE]"));
        assert_eq!(r.next_data().unwrap(), None); // clean EOF
    }

    #[test]
    fn sse_reader_surfaces_io_error_not_clean_eof() {
        // L5: a mid-stream read error (connection reset, truncated body) must
        // NOT be masked as a clean Ok(None) EOF — otherwise the caller would
        // assemble whatever partial text arrived and report it as a successful
        // (possibly empty) completion. It surfaces as Err through next_data,
        // and the SseSource trait maps it to StreamAbort::StreamError so the
        // provider bails and the request is retried as transient.
        struct ErroringRead;
        impl std::io::Read for ErroringRead {
            fn read(&mut self, _b: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "boom"))
            }
        }
        // Raw reader: Err, not Ok(None).
        let mut r = SseReader::new(ErroringRead);
        let err = r.next_data().unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");

        // Through the trait → StreamError (deterministic, no worker thread).
        struct ErroringRead2;
        impl std::io::Read for ErroringRead2 {
            fn read(&mut self, _b: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "cut"))
            }
        }
        // Through the SseSource impl → StreamError (deterministic, no worker).
        let mut r = SseReader::new(ErroringRead2);
        match SseSource::next_payload(&mut r) {
            Err(StreamAbort::StreamError(msg)) => assert!(msg.contains("cut"), "{msg}"),
            other => panic!("expected StreamError, got {other:?}"),
        }
    }

    #[test]
    fn cancellable_sse_surfaces_io_error_through_worker() {
        // L5 (live path): the worker thread reads an erroring reader and must
        // forward it as StreamError via the channel — not let the receiver see
        // a clean Disconnected (EOF) and call the partial stream a success.
        struct ErroringRead;
        impl std::io::Read for ErroringRead {
            fn read(&mut self, _b: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset"))
            }
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let mut s = CancellableSse::new(ErroringRead, cancel, Duration::from_secs(30)).unwrap();
        match s.next_payload() {
            Err(StreamAbort::StreamError(msg)) => assert!(msg.contains("reset"), "{msg}"),
            other => panic!("expected StreamError, got {other:?}"),
        }
    }

    #[test]
    fn cancellable_sse_reads_payloads_then_eof() {
        let raw = b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n";
        let cancel = Arc::new(AtomicBool::new(false));
        let mut s = CancellableSse::new(&raw[..], cancel, Duration::from_secs(30)).unwrap();
        assert_eq!(s.next_payload().unwrap().as_deref(), Some("{\"a\":1}"));
        assert_eq!(s.next_payload().unwrap().as_deref(), Some("{\"b\":2}"));
        assert!(s.next_payload().unwrap().is_none()); // reader hit EOF
    }

    #[test]
    fn cancellable_sse_cancel_breaks_a_stalled_read() {
        // A reader that never produces data and never EOFs → simulates a
        // half-open (stalled) connection. Flipping cancel must break
        // next_payload within ~POLL_SLICE, not the full 30s idle timeout.
        struct Stall;
        impl std::io::Read for Stall {
            fn read(&mut self, _b: &mut [u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_secs(300));
                Ok(0)
            }
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let mut s = CancellableSse::new(Stall, cancel.clone(), Duration::from_secs(30)).unwrap();
        let c = cancel;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            c.store(true, Ordering::Relaxed);
        });
        let start = Instant::now();
        match s.next_payload() {
            Err(StreamAbort::Cancelled) => {
                assert!(start.elapsed() < Duration::from_secs(2), "cancel too slow");
            }
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }
}
