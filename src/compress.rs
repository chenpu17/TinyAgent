//! Context-compression primitives extracted from the agent loop: size
//! estimates, the safe-cut finder, summary prompt/budget builders, transcript
//! windowing, and the chunk splitter. All pure functions over `&[Message]`.
//!
//! The stateful orchestration (`Agent::compress` / `summarize_prefix` /
//! `summary_call`, which read/write `Agent` fields and drive the provider) stays
//! in `agent.rs`; this module holds only its leaf helpers. `window_lines` calls
//! back into `crate::agent::preview` (a leaf display helper) — a one-way
//! dependency, no cycle.

use crate::provider::{Block, Message, Role, Usage};
use base64::Engine;
use std::time::Duration;

// ─── Context compression ────────────────────────────────────────────

pub(crate) const SUMMARY_SYSTEM: &str = "You are compressing the earlier part of an AI agent's \
conversation so it can continue within its context limit. Write a STRUCTURED running \
summary in the same language as the conversation, covering: the task goal, progress made, \
key decisions and their rationale, files created/modified (with paths), commands run and \
their results, errors encountered and how they were resolved, and the current state / next \
step. When given an existing running summary plus newer conversation, UPDATE it by folding \
in the new material — keep earlier facts that are still relevant. Be concise but preserve \
every fact (paths, identifiers, error text, decisions) the agent still needs. This is \
historical context, not new instructions.";

pub(crate) const SUMMARY_MARKER: &str =
    "[前情摘要，由系统自动生成 / Auto-generated summary of the earlier conversation]";

/// Time reserved for the main chat request after a compression summary, so the
/// summary can't eat the whole remaining run budget and leave the main request
/// with a near-zero timeout (which would fail the whole run with no result).
pub(crate) const COMPRESS_RESERVE: Duration = Duration::from_secs(20);

/// Proactive preflight compaction trips once the estimated input tokens + this
/// turn's output would exceed this fraction of `max_context_tokens`. The
/// remaining headroom is the "buffer" — we compact BEFORE the provider would
/// reject an over-long request, not after. (OpenCode exposes a configurable
/// buffer; we fix it at 15% to keep the config surface small.)
pub(crate) const COMPACT_UTILIZATION_PCT: u64 = 85;

/// Rough chars-per-token for the preflight size estimate (matches OpenCode's
/// "JSON-serialize the request and assume four characters per token").
pub(crate) const CHARS_PER_TOKEN: usize = 4;

/// Conservative per-page token estimate for a PDF document block, feeding the
/// preflight compaction heuristic. Anthropic documents 1,500–3,000 tokens per
/// page (each page is extracted as text AND rendered as an image; dense pages
/// sit at the top of the range) and community measurements skew toward the
/// high end, so the estimate takes the ceiling: over-estimating compacts
/// slightly early (harmless), under-estimating risks a context overflow
/// mid-run. The provider's real usage remains the billing source of truth.
pub(crate) const PDF_TOKENS_PER_PAGE: usize = 3_000;

/// Rough page count for a base64-encoded PDF, without a real PDF parser.
/// Best-of three heuristics:
/// 1. plain `/Type /Page` / `/Type/Page` object markers (the common case for
///    LaTeX / browser / Word output, which write page objects uncompressed);
/// 2. the largest `/Count N` within a short window of a `/Type /Pages` node —
///    the page-tree root's count, which still catches documents whose page
///    objects are hidden inside compressed object streams (PDF 1.5+ ObjStm),
///    where a byte scan sees none;
/// 3. if neither yields anything (fully opaque bytes), a size fallback of
///    ~100 KiB/page (typical for text PDFs), floored at 1.
///
/// An estimate only — it feeds [`estimate_tokens`], never gates a request.
pub(crate) fn pdf_page_estimate(b64: &str) -> usize {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .unwrap_or_default();
    if raw.is_empty() {
        return 1;
    }
    let found = count_page_objects(&raw, b"/Type /Page")
        .max(count_page_objects(&raw, b"/Type/Page"))
        .max(pages_tree_count(&raw));
    if found > 0 {
        found
    } else {
        (raw.len() / (100 * 1024)).max(1)
    }
}

/// Count occurrences of a `needle` (`/Type /Page` or `/Type/Page`) that don't
/// extend into a longer name: the byte after a hit must not be alphanumeric.
/// `/Type /Pages` (the parent tree node, not a page) and `/PageMode`-style
/// longer names are thereby rejected.
fn count_page_objects(hay: &[u8], needle: &[u8]) -> usize {
    let mut count = 0;
    let mut from = 0;
    while hay.len() - from >= needle.len() {
        let Some(rel) = hay[from..].windows(needle.len()).position(|w| w == needle) else {
            break;
        };
        let end = from + rel + needle.len();
        if !hay.get(end).is_some_and(|c| c.is_ascii_alphanumeric()) {
            count += 1;
        }
        from = end;
    }
    count
}

/// Largest `/Count N` within a short window around a `/Type /Pages` node. The
/// page-tree root's `/Count` is the document's page total (intermediate nodes
/// carry subtree counts ≤ it). Restricting the scan to this window keeps
/// outline/bookmark `/Count` entries — which count visible items, not pages —
/// out of the max.
fn pages_tree_count(hay: &[u8]) -> usize {
    let mut best = 0;
    for needle in [b"/Type /Pages".as_slice(), b"/Type/Pages".as_slice()] {
        let mut from = 0;
        while hay.len() - from >= needle.len() {
            let Some(rel) = hay[from..].windows(needle.len()).position(|w| w == needle) else {
                break;
            };
            let node = from + rel;
            // A dict's key order is arbitrary, so look slightly BACKWARD too
            // (`<< /Count 3 /Type /Pages … >>`); 64 bytes can't reach past the
            // previous object's `/Count` in any realistic layout.
            let lo = node.saturating_sub(64);
            // Forward: bound the window by the SAME object — a dict's /Count
            // sits inside the object declaring /Type /Pages, so cut at its
            // `endobj` (64 KiB sanity cap if malformed input has none). That
            // is wide enough for any /Kids array (~7 bytes per page ref ⇒
            // ~4 KiB at the API's 600-page cap) AND immune to outline /
            // bookmark /Count entries, which live in DIFFERENT objects.
            let hi = hay[node..]
                .windows(6)
                .position(|w| w == b"endobj")
                .map(|p| node + p)
                .unwrap_or((node + 64 * 1024).min(hay.len()))
                .min(hay.len());
            if let Some(n) = count_in_window(&hay[lo..hi]) {
                best = best.max(n);
            }
            from = node + needle.len();
        }
    }
    best
}

/// First `/Count N` (N decimal) inside a window, if any.
fn count_in_window(win: &[u8]) -> Option<usize> {
    let idx = win.windows(6).position(|w| w == b"/Count")?;
    let mut j = idx + 6;
    while matches!(win.get(j), Some(b' ' | b'\r' | b'\n' | b'\t')) {
        j += 1;
    }
    let start = j;
    while matches!(win.get(j), Some(c) if c.is_ascii_digit()) {
        j += 1;
    }
    if j == start {
        return None;
    }
    std::str::from_utf8(&win[start..j]).ok()?.parse().ok()
}

/// A transcript larger than this is summarized in message-aligned chunks then
/// combined, bounding the per-call compression ratio so detail isn't crushed
/// (~6k tokens/chunk → a ~6:1 summary ratio). Approximate byte limit.
pub(crate) const SUMMARY_CHUNK_CHARS: usize = 24_000;

/// Per-chunk summary output budget. Each chunk is ≈6k tokens, so 1024 output
/// tokens keeps the chunk ratio near 6:1 (the same target as the proportional
/// single-call budget).
pub(crate) const SUMMARY_CHUNK_BUDGET: u32 = 1024;

/// Phrases a provider embeds in its error body when the request exceeds the
/// model's context window. Both providers surface the raw HTTP body in the
/// anyhow error's Display (`openai.rs` "…API error 400: {text}",
/// `anthropic.rs` "…API error {status}: {text}"), so a case-blind substring
/// match is enough. The OpenAI 400-fix path (`diagnose_400`/`apply_fix`) has
/// already absorbed the max_tokens / stream_options / reasoning_effort 400s, so
/// only genuine overflow reaches here.
///
/// Kept SPECIFIC: a non-overflow error misread as overflow wastes one
/// force-compress + retry before the real error surfaces, so vague phrases
/// like bare "context length" or "reduce the length" are excluded. A MISS is
/// safe — the error just propagates and the caller sees it. Covers OpenAI /
/// Anthropic / DeepSeek / vLLM / Ollama.
pub(crate) const OVERFLOW_PHRASES: &[&str] = &[
    "context_length_exceeded",     // OpenAI error type
    "maximum context length",      // OpenAI message body
    "prompt is too long",          // Anthropic
    "too many input tokens",       // Anthropic variant / generic
    "input length and max_tokens", // Anthropic "input length + max_tokens exceed context"
    "input too long",              // some OpenAI-compatible gateways
    "exceeds the context window",  // vLLM / Ollama / gateways
                                   // NOTE: bare "token limit exceeded" is intentionally NOT here — some
                                   // gateways use it for context overflow, but it ALSO appears in OUTPUT-token-
                                   // limit errors, which compressing won't fix (the failure would repeat on the
                                   // shorter history, wasting one force-compress + retry). It's matched with a
                                   // guard in `is_context_overflow` instead.
];

/// Rough size estimate of a conversation (chars; images count cheaply).
pub(crate) fn total_chars(messages: &[Message]) -> usize {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .map(|b| match b {
            Block::Text(t) => t.len(),
            Block::Image { data, .. } => data.len() / 100, // base64 is ~33% overhead
            // Char floor (same heuristic as images) — the legacy
            // `compress_threshold_chars` gate's only signal for documents, and
            // a small conservative extra on top of the page-aware estimate in
            // `estimate_tokens` (both over-estimate, which is the safe
            // direction for a compaction preflight).
            Block::Document { data, .. } => data.len() / 100,
            Block::ToolUse { input, .. } => input.to_string().len(),
            // A tool result may carry an image payload (e.g. load_image).
            // Count it like a top-level image, or a load_image-heavy
            // conversation never trips the compress threshold.
            Block::ToolResult { content, image, .. } => {
                let img = image
                    .as_deref()
                    .map(|b| match b {
                        Block::Image { data, .. } => data.len() / 100,
                        _ => 0,
                    })
                    .unwrap_or(0);
                content.len() + img
            }
        })
        .sum()
}

/// Rough token estimate of what the next request will cost the model: message
/// content + system prompt + advertised tool schemas, divided by the
/// chars-per-token heuristic. Used ONLY for the preflight "should I compact
/// before this request?" decision — never billed or reported; the provider's
/// real usage is the source of truth for accounting.
pub(crate) fn estimate_tokens(
    messages: &[Message],
    system: Option<&str>,
    tool_schema_chars: usize,
) -> usize {
    let chars = total_chars(messages) + system.map(|s| s.len()).unwrap_or(0) + tool_schema_chars;
    // PDF document blocks are estimated per PAGE (Anthropic processes each
    // page as text + image, 1.5–3k tokens/page), not by payload size — a
    // 10 MiB text PDF is hundreds of pages, which any bytes-per-token
    // heuristic grossly under-counts. `pdf_page_estimate` re-decodes and
    // re-scans per call: the preflight runs once per turn and the scan is
    // memory-bandwidth-bound (~ms even at the 10 MiB cap), so no cache is
    // worth the state it would add to the plain-data Block.
    let pdf_tokens: usize = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            Block::Document { data, .. } => Some(pdf_page_estimate(data)),
            _ => None,
        })
        .sum::<usize>()
        .saturating_mul(PDF_TOKENS_PER_PAGE);
    chars / CHARS_PER_TOKEN + pdf_tokens
}

/// True if a provider error looks like "the request was too long for the
/// model's context window" — i.e. compressing and retrying might let it fit.
/// See `OVERFLOW_PHRASES` for the unambiguous match surface, plus the guarded
/// "token limit exceeded" below.
pub(crate) fn is_context_overflow(e: &anyhow::Error) -> bool {
    let msg = format!("{e:#}").to_lowercase();
    if OVERFLOW_PHRASES.iter().any(|p| msg.contains(p)) {
        return true;
    }
    // "token limit exceeded" is ambiguous: some gateways emit it for INPUT
    // context overflow (compressing helps), but it also appears in OUTPUT-token-
    // limit errors (compressing does NOT help — the failure repeats on the
    // shorter history, wasting one force-compress + retry). Match it only when
    // the message doesn't name an output/completion context.
    msg.contains("token limit exceeded") && !msg.contains("output") && !msg.contains("completion")
}

/// Build the compressed history: original task (with the summary folded in
/// as one extra text block) + the untouched tail. Folding into message[0]
/// keeps proper user → assistant alternation when the tail starts with an
/// assistant message. Previous summary blocks are replaced, not stacked —
/// the new summary already absorbed them via the transcript.
pub(crate) fn fold_summary(messages: &[Message], cut: usize, summary: &str) -> Vec<Message> {
    // Exact count: one retained head + the tail `messages[cut..]`. Saturating
    // so a pathological cut (find_safe_cut guarantees cut>=3, but don't rely on
    // an invariant upstream to avoid a usize underflow here) can't underflow.
    let mut compressed = Vec::with_capacity(messages.len().saturating_sub(cut) + 1);
    // The head MUST be a user message so the folded history keeps a valid
    // user→assistant→… cadence (Anthropic rejects an assistant-led exchange).
    // Normally messages[0] is the opening user turn, but a resumed/edited
    // session can start with an assistant message — fold the summary into a
    // fresh empty user head then, rather than grafting it onto an assistant.
    // `first()` (not `[0]`): this is pub(crate) and a caller handing in an
    // empty slice must get the fresh-head fallback, not an index panic.
    let mut head = match messages.first() {
        Some(m) if m.role == Role::User => {
            let mut h = m.clone();
            h.content
                .retain(|b| !matches!(b, Block::Text(t) if t.starts_with(SUMMARY_MARKER)));
            h
        }
        _ => Message::user(Vec::new()),
    };
    head.content
        .push(Block::Text(format!("{SUMMARY_MARKER}\n{summary}")));
    compressed.push(head);
    compressed.extend_from_slice(&messages[cut..]);
    compressed
}

/// Find a safe tail-start index. An assistant message is always a safe
/// boundary: its tool_use blocks (if any) are answered by the tool_result
/// that immediately follows it inside the kept tail. Cutting before a
/// tool_result-bearing user message would orphan those results, so those
/// are skipped. Must leave at least one message worth summarizing beyond
/// the retained head, i.e. n must exceed keep_recent by at least 2.
pub(crate) fn find_safe_cut(messages: &[Message], keep_recent: usize) -> Option<usize> {
    let n = messages.len();
    // Saturating guard: a pathological keep_recent (usize::MAX) must disable
    // compression, not wrap the subtraction below (release: giant cut that
    // panics on slice; debug: add overflow panic right here).
    if n <= keep_recent.saturating_add(2) {
        return None;
    }
    // Scan from the tail's nominal start upward for an assistant boundary.
    let mut cut = n - keep_recent;
    if cut >= n {
        // keep_recent == 0: the nominal tail start is n — an EMPTY tail,
        // which is a legal cut. fold_summary builds head + messages[n..],
        // i.e. everything except the retained head folds into the summary.
        // The real loop's history is always user-final at request points, so
        // the old clamp-to-n-1 fallback made the boundary scan miss and
        // silently DISABLED compression for keep_recent=0 configs.
        return Some(n);
    }
    while cut < n {
        if messages[cut].role == Role::Assistant {
            return Some(cut);
        }
        cut += 1;
    }
    None
}

/// Pull the previous rolling summary out of the head message (if any). The
/// summary is stored as a Text block beginning with SUMMARY_MARKER (see
/// fold_summary); on a later compression we UPDATE it rather than re-feed
/// it as raw transcript, so the model refines a running brief instead of
/// re-summarizing its own output every time.
pub(crate) fn extract_running_summary(messages: &[Message]) -> Option<String> {
    let head = messages.first()?;
    head.content.iter().find_map(|b| match b {
        Block::Text(t) if t.starts_with(SUMMARY_MARKER) => {
            // Strip the marker line; keep the summary body.
            Some(
                t[SUMMARY_MARKER.len()..]
                    .trim_start_matches('\n')
                    .to_string(),
            )
        }
        _ => None,
    })
}

/// Format each block of `messages[..cut]` (skipping the head's running-
/// summary block) as a transcript line. Shared by the single-call and
/// chunked summarize paths so the formatting lives in one place.
pub(crate) fn window_lines(messages: &[Message], cut: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for (i, m) in messages[..cut].iter().enumerate() {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        for b in &m.content {
            // Skip the head's running-summary block — it's the thing we're
            // updating, not raw conversation to summarize.
            if i == 0 {
                if let Block::Text(t) = b {
                    if t.starts_with(SUMMARY_MARKER) {
                        continue;
                    }
                }
            }
            let line = match b {
                Block::Text(t) => format!("[{role}] {t}\n"),
                Block::Image { .. } => format!("[{role}] <image>\n"),
                Block::Document { .. } => format!("[{role}] <document>\n"),
                Block::ToolUse { name, input, .. } => {
                    format!(
                        "[{role}] tool_call {name}: {}\n",
                        crate::agent::preview(&input.to_string())
                    )
                }
                Block::ToolResult {
                    content, is_error, ..
                } => {
                    let body = crate::agent::preview(content);
                    let tag = if *is_error {
                        "tool_error"
                    } else {
                        "tool_result"
                    };
                    format!("[{role}] {tag}: {body}\n")
                }
            };
            lines.push(line);
        }
    }
    lines
}

/// Proportional summary output budget: ≈16 bytes of transcript per output
/// token, clamped to [1024, 4096]. 100k bytes → 4096 tokens (~6:1), a small
/// window → 1024. Replaces the old fixed 2048, which crushed a 100k-char
/// prefix at ~12:1 and dropped coding-relevant detail.
pub(crate) fn summary_budget(size: usize) -> u32 {
    // try_from, not `as`: `as` truncates (usize→u32), so a transcript over
    // 16 GiB would wrap to a small budget. Unreachable in practice —
    // defensive, same spirit as the saturating usage adds.
    u32::try_from(size / 16)
        .unwrap_or(u32::MAX)
        .clamp(1024, 4096)
}

/// Build the user message for a summary call. With an `old_summary` we ask the
/// model to UPDATE a running brief from new material; otherwise to summarize
/// fresh. The system prompt (SUMMARY_SYSTEM) is shared.
pub(crate) fn summary_prompt(body: &str, old_summary: Option<&str>) -> String {
    match old_summary {
        Some(old) if !old.trim().is_empty() => format!(
            "Below is the current running summary of the earlier conversation, followed by newer \
             conversation. Produce an UPDATED running summary that folds in the new material while \
             keeping earlier facts that are still relevant.\n\n\
             ## Current running summary\n{old}\n\n## Newer conversation\n{body}"
        ),
        _ => format!("Summarize this conversation transcript:\n\n{body}"),
    }
}

/// Split transcript lines into chunks each ≤ `max_bytes`, breaking only at
/// line boundaries (so a tool_use/tool_result block stays in one chunk) — EXCEPT
/// a single line larger than `max_bytes` (e.g. a huge read_file/shell dump, or
/// an untruncated user paste) is itself split at char boundaries. Without that,
/// one oversized chunk's summary call could itself overflow the window,
/// defeating the overflow-recovery path that force-compresses exactly when the
/// context is huge.
pub(crate) fn chunk_lines(lines: Vec<String>, max_bytes: usize) -> Vec<String> {
    // First break any over-long line into ≤ max_bytes pieces.
    let mut pieces: Vec<String> = Vec::with_capacity(lines.len());
    for line in lines {
        if line.len() <= max_bytes {
            pieces.push(line);
        } else {
            split_long(&line, max_bytes, &mut pieces);
        }
    }
    // Then pack pieces into chunks at piece boundaries.
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for piece in pieces {
        if !cur.is_empty() && cur.len() + piece.len() > max_bytes {
            chunks.push(std::mem::take(&mut cur));
        }
        cur.push_str(&piece);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// Split an over-long line into ≤ `max_bytes` pieces at char boundaries. The
/// split may fall inside a transcript line's prefix tag, but in the summary
/// path that's acceptable — we trade block atomicity for a bounded chunk size.
fn split_long(line: &str, max_bytes: usize, out: &mut Vec<String>) {
    let mut buf = String::new();
    for c in line.chars() {
        if !buf.is_empty() && buf.len() + c.len_utf8() > max_bytes {
            out.push(std::mem::take(&mut buf));
        }
        buf.push(c);
    }
    if !buf.is_empty() {
        out.push(buf);
    }
}

/// Add two optional per-request usages into one. Either side may be `None`
/// (provider reported no usage); the result carries whichever was billed.
pub(crate) fn merge_usage(a: Option<Usage>, b: Option<Usage>) -> Option<Usage> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x),
        (Some(mut x), Some(y)) => {
            // Saturating: a lying provider report must not panic (debug) /
            // wrap to a small total (release) the summary-cost accumulator.
            x.input_tokens = x.input_tokens.saturating_add(y.input_tokens);
            x.output_tokens = x.output_tokens.saturating_add(y.output_tokens);
            x.cache_creation_input_tokens = x
                .cache_creation_input_tokens
                .saturating_add(y.cache_creation_input_tokens);
            x.cache_read_input_tokens = x
                .cache_read_input_tokens
                .saturating_add(y.cache_read_input_tokens);
            Some(x)
        }
    }
}
