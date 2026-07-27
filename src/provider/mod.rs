mod anthropic;
mod openai;
#[cfg(test)]
pub(crate) mod mock;

pub use anthropic::Anthropic;
pub use openai::OpenAi;

use crate::config::{Config, ProviderKind};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

/// One block of message content. Mirrors Anthropic's content-block model;
/// the OpenAI provider maps these onto its own wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input object.
    pub schema: Value,
}

/// Incremental events emitted while streaming a response.
pub enum ChatEvent {
    TextDelta(String),
}

/// Token usage for one chat. Best-effort: None when the server didn't report
/// (request errored, or the stream was cut off before the usage frame).
#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// A completed assistant turn.
pub struct ChatResponse {
    pub blocks: Vec<Block>,
    /// "end_turn" | "tool_use" | "max_tokens" | provider-specific value.
    pub stop_reason: String,
    pub usage: Option<Usage>,
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
}

pub trait Provider: Send + Sync {
    /// Send one chat request, streaming text deltas to `on_event`.
    /// Returns the fully-assembled assistant message.
    fn chat(&self, req: &ChatRequest, on_event: &mut dyn FnMut(ChatEvent)) -> Result<ChatResponse>;
}

pub fn build(cfg: &Config, api_key: &str, client: reqwest::blocking::Client) -> Box<dyn Provider> {
    let base = cfg.base_url();
    match cfg.provider {
        ProviderKind::Anthropic => Box::new(Anthropic::new(client, base, api_key)),
        ProviderKind::Openai => Box::new(OpenAi::new(client, base, api_key)),
    }
}

/// HTTP client with explicit timeouts. A stuck or trickling connection
/// must never outlive the whole run budget — an agent that hangs forever
/// would stall any CI/eval pipeline it runs in. Clones share one
/// connection pool (cheap, Arc inside).
pub fn new_client(run_timeout_secs: u64) -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(run_timeout_secs.max(60)))
        // Don't follow redirects: a custom base_url that redirects could leak
        // the API key header (x-api-key / Authorization) to a third party.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
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

    /// Returns the next `data:` payload, or None at end of stream.
    pub fn next_data(&mut self) -> Option<String> {
        let mut line = String::new();
        loop {
            line.clear();
            match self.reader.read_line(&mut line) {
                Ok(0) => return None,
                Ok(_) => {
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    if let Some(data) = trimmed.strip_prefix("data:") {
                        return Some(data.trim_start().to_string());
                    }
                    // skip event:, id:, comments and blank lines
                }
                Err(_) => return None,
            }
        }
    }
}

/// Why a cancellable stream read stopped early. `SseReader` (sync) never
/// produces these; only `CancellableSse` does.
#[derive(Debug, Clone, Copy)]
pub(crate) enum StreamAbort {
    /// Caller flipped the shared cancel flag (Esc/Ctrl-C in the TUI).
    Cancelled,
    /// No new chunk for `STREAM_IDLE_TIMEOUT` — the connection is half-open
    /// (server stopped sending without closing). Distinct from the reqwest
    /// total timeout, which is a backstop for the whole request.
    IdleTimeout,
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
        Ok(self.next_data())
    }
}

/// No new chunk for this long ⇒ treat the stream as stalled. Tuned for
/// ordinary chat models whose inter-chunk gap is sub-second; deep-reasoning
/// models with long first-token latency may need this raised. The reqwest
/// total timeout (`limits.timeout_secs`) remains the hard backstop.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Cancel/idle poll granularity. Bounds how long after Esc the stream read
/// actually returns — small enough to feel instant, large enough to avoid
/// needless wakeups on a healthy stream.
const POLL_SLICE: Duration = Duration::from_millis(200);

/// Wraps a blocking reader in a worker thread, exposing payloads with an idle
/// timeout and cancel check. The reader thread can't observe cancel (it's
/// blocked in `read`), so on cancel/timeout the caller stops polling and the
/// thread lingers until reqwest's per-request timeout aborts the underlying
/// read (then it exits) or the process exits. Bounded, no busy-wait.
pub(crate) struct CancellableSse {
    rx: mpsc::Receiver<String>,
    cancel: Arc<AtomicBool>,
}

impl CancellableSse {
    pub(crate) fn new<R: Read + Send + 'static>(reader: R, cancel: Arc<AtomicBool>) -> Self {
        let (tx, rx) = mpsc::channel::<String>();
        thread::Builder::new()
            .name("sse-reader".into())
            .spawn(move || {
                let mut sse = SseReader::new(reader);
                while let Some(d) = sse.next_data() {
                    if tx.send(d).is_err() {
                        break; // caller dropped the receiver → stop pumping
                    }
                }
                // tx drops here → receiver sees Disconnected (end of stream).
            })
            .expect("spawn sse-reader thread");
        Self { rx, cancel }
    }
}

impl SseSource for CancellableSse {
    fn next_payload(&mut self) -> std::result::Result<Option<String>, StreamAbort> {
        // Poll in short slices so a cancel flip (Esc/Ctrl-C) is noticed within
        // ~POLL_SLICE, not only when the idle timeout eventually fires. A
        // stuck connection still hits IdleTimeout after STREAM_IDLE_TIMEOUT.
        let start = Instant::now();
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(StreamAbort::Cancelled);
            }
            if start.elapsed() >= STREAM_IDLE_TIMEOUT {
                return Err(StreamAbort::IdleTimeout);
            }
            match self.rx.recv_timeout(POLL_SLICE) {
                Ok(d) => return Ok(Some(d)),
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
        assert_eq!(r.next_data().as_deref(), Some("{\"a\":1}"));
        assert_eq!(r.next_data().as_deref(), Some("{\"b\":2}"));
        assert_eq!(r.next_data().as_deref(), Some("[DONE]"));
        assert_eq!(r.next_data(), None);
    }

    #[test]
    fn cancellable_sse_reads_payloads_then_eof() {
        let raw = b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n";
        let cancel = Arc::new(AtomicBool::new(false));
        let mut s = CancellableSse::new(&raw[..], cancel);
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
        let mut s = CancellableSse::new(Stall, cancel.clone());
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
