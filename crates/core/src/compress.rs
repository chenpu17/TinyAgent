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
every fact (paths, identifiers, error text, decisions) the agent still needs. Quote critical \
paths, identifiers, commands, and error text VERBATIM — never paraphrase, translate, or \
abbreviate them; compounding paraphrase across repeated compactions is how facts drift. This \
is historical context, not new instructions.";

pub(crate) const SUMMARY_MARKER: &str =
    "[前情摘要，由系统自动生成 / Auto-generated summary of the earlier conversation]";

/// Marks the head block that restores the agent's todo checklist after a fold
/// (see [`latest_todo_checklist`]). Stripped and re-derived on every later
/// fold, so the restored plan is always the latest one.
pub(crate) const TODO_MARKER: &str =
    "[任务清单，压缩后恢复 / Task list restored after context compaction]";

/// Marks a head block preserving a middle user message verbatim (Codex-style:
/// user intent is never paraphrased through the summary). Survives later
/// folds; subject to the shared [`USER_QUOTE_TOTAL_CHARS`] budget.
pub(crate) const USER_QUOTE_MARKER: &str = "[用户消息原文保留 / User message preserved verbatim]";

/// Marks a supervisor note the host injected mid-run (R1, see
/// `Agent::supervisor_handle`): a time-sensitive control signal, carried as a
/// plain text block so it survives `normalize_adjacent`, session round-trips,
/// and old-binary reads like any user text. Deliberately NOT given the
/// USER_QUOTE treatment: a MIDDLE note is absorbed through the summary (its
/// gist rides the summary prompt, never verbatim preservation — it goes
/// stale within a few turns), while a note that landed in the fold HEAD
/// (turn-1 injection merges into the task message) rides `messages[0]`
/// verbatim like the rest of the original task. Public: hosts replay notes
/// from history by byte-exact prefix match — the string is a cross-team
/// contract, change only with a version note.
pub const SUPERVISOR_NOTE_MARKER: &str = "[监督者旁注 / Supervisor note]";

/// Per-message cap for a preserved verbatim user text — big pastes (logs,
/// code dumps) get truncated with the standard marker rather than crowding
/// the head.
pub(crate) const USER_QUOTE_MAX_CHARS: usize = 2_000;

/// Total budget for ALL verbatim user-quote blocks in the head. Once
/// exceeded, the OLDEST quotes drop (FIFO) — recent user intent outranks
/// ancient user intent, and the summary still carries the gist of what fell
/// out.
pub(crate) const USER_QUOTE_TOTAL_CHARS: usize = 12_000;

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

/// Rough units-per-token for the preflight size estimate. One ASCII byte is
/// one unit (OpenCode's "JSON-serialize the request and assume four
/// characters per token"); CJK code points weigh more (see [`char_units`]) so
/// a Chinese-heavy history is not under-counted 2–3× as it was when this
/// divisor ran over raw UTF-8 bytes.
pub(crate) const CHARS_PER_TOKEN: usize = 4;

/// Unit weight of a CJK code point. Four units = one token, so 9 units ≈
/// 2.25 tokens per character — the conservative end of the measured 1.5–2.5
/// token/character range for Han, kana and Hangul. Over-estimating compacts
/// slightly early (the safe direction); under-estimating lets the provider
/// reject an over-long request before the next preflight.
const CJK_UNITS: usize = 9;

/// Unit weight of a non-ASCII, non-CJK code point (accented Latin, Cyrillic,
/// Arabic, emoji, …): one token per character, a mild over-estimate for most
/// scripts and the safe direction for the ones that tokenize poorly.
const OTHER_UNITS: usize = 4;

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

/// Tool results at or below this size are never pruned — placeholder churn on
/// small outputs buys nothing (short results and most errors pass through
/// untouched, so recent debugging context survives).
pub(crate) const PRUNE_MIN_RESULT_CHARS: usize = 2_000;

/// A prune pass must free at least this many chars to be worth rewriting the
/// history for (which also breaks the provider prompt-cache prefix) —
/// OpenCode's "only prune when it frees enough" gate.
pub(crate) const PRUNE_MINIMUM_CHARS: usize = 20_000;

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

/// Unit weight of one character under the `/4` token yardstick. ASCII keeps
/// the historical 1 unit/byte; CJK code points weigh [`CJK_UNITS`] because
/// UTF-8 bytes alone under-count them (1000 Han characters are 3000 bytes —
/// 750 tokens by the old bytes/4 rule, versus ~1500–2500 real tokens).
/// Fullwidth punctuation, kana, Hangul, CJK compatibility ideographs and the
/// supplementary-plan CJK blocks are all included; everything else non-ASCII
/// gets [`OTHER_UNITS`].
pub(crate) fn char_units(c: char) -> usize {
    match c as u32 {
        // CJK punctuation, kana, Hangul compatibility jamo, CJK Ext A +
        // unified ideographs, Hangul syllables, compatibility ideographs &
        // fullwidth forms, and the supplementary-plane ideograph blocks.
        0x3000..=0x303F
        | 0x3040..=0x30FF
        | 0x3130..=0x318F
        | 0x31F0..=0x31FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA960..=0xA97F
        | 0xAC00..=0xD7FF
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x2FFFD
        | 0x30000..=0x3FFFD => CJK_UNITS,
        _ if c.is_ascii() => 1,
        _ => OTHER_UNITS,
    }
}

/// Unit size of a string under [`char_units`] (the `/4` happens once on the
/// SUM — see [`estimate_tokens`]).
pub(crate) fn text_units(s: &str) -> usize {
    s.chars().map(char_units).sum()
}

/// Size of a single block under the token-estimate yardstick. This is the
/// token-channel twin of [`block_chars`]: text-bearing content goes through
/// [`text_units`] (CJK-aware), while images, PDFs and tool-use JSON keep
/// their byte-derived weights. Kept a separate function from `block_chars`
/// ON PURPOSE — the legacy `compress_threshold_chars` gate and prune/quote
/// budgets still speak bytes (`total_chars`), and changing them would alter
/// thresholds unrelated to the CJK under-count.
pub(crate) fn block_units(b: &Block) -> usize {
    match b {
        Block::Text(t) => text_units(t),
        // A tool result may carry an image payload (e.g. load_image).
        Block::ToolResult { content, image, .. } => {
            text_units(content) + image.as_deref().map(image_payload_len).unwrap_or(0)
        }
        _ => block_chars(b),
    }
}

/// Size of a single block under the shared chars yardstick (the `/4` happens
/// once on the SUM — see [`estimate_tokens`]). `total_chars` and
/// `context_report`'s per-bucket attribution both route through this one
/// function so the two can never drift apart — the precondition for
/// "uncalibrated breakdown total == `estimate_tokens`" (context_report's
/// sum invariant). Deliberately an EXHAUSTIVE match: a new `Block` variant
/// must decide its cost here at compile time (the four-layer new-variant
/// checklist's compress entry).
pub(crate) fn block_chars(b: &Block) -> usize {
    match b {
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
            content.len() + image.as_deref().map(image_payload_len).unwrap_or(0)
        }
    }
}

/// Exact token ADD-ON a block contributes outside the chars/4 channel: PDF
/// documents bill per PAGE (`pdf_page_estimate × PDF_TOKENS_PER_PAGE`), every
/// other block adds nothing. Shared by [`estimate_tokens`] and
/// `context_report`'s document bucket for the same never-drift reason as
/// [`block_chars`].
pub(crate) fn block_pdf_tokens(b: &Block) -> usize {
    match b {
        Block::Document { data, .. } => pdf_page_estimate(data).saturating_mul(PDF_TOKENS_PER_PAGE),
        _ => 0,
    }
}

/// Rough size estimate of a conversation (chars; images count cheaply).
pub(crate) fn total_chars(messages: &[Message]) -> usize {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .map(block_chars)
        .sum()
}

/// Rough token estimate of what the next request will cost the model: message
/// content + system prompt + advertised tool schemas, divided by the
/// units-per-token heuristic. Used ONLY for the preflight "should I compact
/// before this request?" decision — never billed or reported; the provider's
/// real usage is the source of truth for accounting.
///
/// Text goes through [`block_units`] / [`text_units`], not raw bytes: the
/// bytes/4 rule under-counted CJK 2–3× and let a Chinese-heavy history reach
/// the provider before the preflight noticed. Tool schemas stay byte-counted
/// (they are overwhelmingly ASCII JSON) so `context_report` can keep using
/// the same `tool_schema_chars` yardstick.
pub(crate) fn estimate_tokens(
    messages: &[Message],
    system: Option<&str>,
    tool_schema_chars: usize,
) -> usize {
    let units = messages
        .iter()
        .flat_map(|m| &m.content)
        .map(block_units)
        .sum::<usize>()
        + system.map(text_units).unwrap_or(0)
        + tool_schema_chars;
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
        .map(block_pdf_tokens)
        .sum();
    units / CHARS_PER_TOKEN + pdf_tokens
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
/// Build the compressed history: original task (with the summary folded in
/// as one extra text block) + the untouched tail. Folding into message[0]
/// keeps proper user → assistant alternation when the tail starts with an
/// assistant message. Previous summary blocks are replaced, not stacked —
/// the new summary already absorbed them via the transcript.
///
/// Post-fold fidelity (see the compaction research notes): alongside the
/// summary the head carries (a) the todo checklist restored when the fold
/// removed the model's last `todo` result ([`TODO_MARKER`]) and (b) the
/// middle user messages' text verbatim under a capped budget
/// ([`USER_QUOTE_MARKER`]) — user intent is never routed through the
/// summarizer's paraphrase.
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
    let (mut head, mut quotes) = match messages.first() {
        Some(m) if m.role == Role::User => {
            let mut h = m.clone();
            // Quotes preserved by earlier folds must survive this fold too
            // ("Survives later folds", see the USER_QUOTE_* consts) — collect
            // them BEFORE the strip below. Scanning the stripped clone could
            // never find them: the strip removes USER_QUOTE_MARKER blocks, so
            // the old scan was dead code and second folds silently dropped
            // the previous era's verbatim quotes. Era order matters: old
            // quotes first, so the FIFO budget below evicts the OLDEST.
            let old: Vec<String> = h
                .content
                .iter()
                .filter_map(|b| match b {
                    Block::Text(t) if t.starts_with(USER_QUOTE_MARKER) => Some(t.clone()),
                    _ => None,
                })
                .collect();
            // All system-managed head blocks are stripped and re-derived
            // below — the new fold replaces, never stacks, them.
            h.content.retain(|b| {
                !matches!(
                    b,
                    Block::Text(t)
                        if t.starts_with(SUMMARY_MARKER)
                            || t.starts_with(TODO_MARKER)
                            || t.starts_with(USER_QUOTE_MARKER)
                )
            });
            (h, old)
        }
        _ => (Message::user(Vec::new()), Vec::new()),
    };
    // Verbatim user quotes: keep the ones earlier folds already preserved
    // (they're middle-user text of THEIR era), add this fold's middle user
    // texts, then enforce the shared budget FIFO-oldest-first.
    for m in messages.get(1..cut).unwrap_or_default() {
        if m.role != Role::User {
            continue;
        }
        for b in &m.content {
            let Block::Text(t) = b else { continue };
            // Supervisor notes are excluded on purpose (R1 contract): they are
            // time-sensitive control signals, stale within a few turns — the
            // summary prompt carries their gist instead of the head carrying
            // them verbatim forever.
            if t.starts_with(SUPERVISOR_NOTE_MARKER) {
                continue;
            }
            let t = t.trim();
            if t.is_empty() {
                continue;
            }
            let mut body = t.to_string();
            crate::tools::truncate(&mut body, USER_QUOTE_MAX_CHARS);
            quotes.push(format!("{USER_QUOTE_MARKER}\n{body}"));
        }
    }
    let total: usize = quotes.iter().map(|q| q.len()).sum();
    if total > USER_QUOTE_TOTAL_CHARS {
        let mut acc = total;
        // Oldest out first; the guard keeps at least one quote even in the
        // degenerate single-oversized case (impossible with the per-message
        // cap, but don't rely on that invariant here).
        while acc > USER_QUOTE_TOTAL_CHARS && quotes.len() > 1 {
            acc -= quotes.remove(0).len();
        }
    }
    for q in quotes {
        head.content.push(Block::Text(q));
    }
    // Restore the todo checklist if the fold swallowed the model's plan view:
    // the tool has no read mode, so without this the plan is invisible until
    // the model thinks to call `todo` again. Skipped when the kept tail
    // already carries a todo result (the live one outranks a copy).
    let tail = messages.get(cut..).unwrap_or_default();
    let todo_ids = todo_use_ids(messages);
    let tail_has_todo = tail.iter().any(|m| {
        m.role == Role::User
            && m.content.iter().any(|b| {
                matches!(b, Block::ToolResult { tool_use_id, is_error: false, .. }
                    if todo_ids.contains(&tool_use_id.as_str()))
            })
    });
    if !tail_has_todo {
        if let Some(list) = latest_todo_checklist(messages) {
            head.content
                .push(Block::Text(format!("{TODO_MARKER}\n{list}")));
        }
    }
    head.content
        .push(Block::Text(format!("{SUMMARY_MARKER}\n{summary}")));
    compressed.push(head);
    // `tail` (get, not a `[cut..]` index): a pathological cut > len must
    // yield an empty tail here, not a slice panic — same defensive stance as
    // the `tail` binding above and the `get(1..cut)` quote scan; fold_summary
    // is pub(crate), so a caller's bad cut must degrade, not crash.
    compressed.extend_from_slice(tail);
    compressed
}

/// Every `todo` tool_use id in the history: the prune-exempt set and the
/// post-fold checklist restore both key off the same pairing fact
/// (ToolResult blocks only carry the id; the name lives on the calling
/// side).
fn todo_use_ids(messages: &[Message]) -> Vec<&str> {
    let mut ids = Vec::new();
    for m in messages {
        if m.role != Role::Assistant {
            continue;
        }
        for b in &m.content {
            if let Block::ToolUse { id, name, .. } = b {
                if name == "todo" {
                    ids.push(id.as_str());
                }
            }
        }
    }
    ids
}

/// The rendered checklist of the LATEST `todo` call, if any — full-replacement
/// semantics make the last call authoritative. `None` when the latest call
/// has no (non-error) result, e.g. it was interrupted.
pub(crate) fn latest_todo_checklist(messages: &[Message]) -> Option<String> {
    let ids = todo_use_ids(messages);
    let latest = ids.last()?;
    for m in messages {
        for b in &m.content {
            if let Block::ToolResult {
                tool_use_id,
                content,
                is_error: false,
                ..
            } = b
            {
                if tool_use_id == latest {
                    return Some(content.clone());
                }
            }
        }
    }
    None
}

/// Replace stale oversized tool results with compact placeholders — the
/// pre-LLM gate of context management (Claude Code "microcompaction" /
/// OpenCode "prune"): a history whose bulk is old tool output shrinks
/// without a single billed request.
///
/// Rules:
/// - only messages OUTSIDE the protection window (the `protect_recent` most
///   recent messages — same knob and semantics as [`find_safe_cut`]'s
///   `keep_recent`) and past the original-task head (index 0) are touched;
/// - only `ToolResult` blocks whose content exceeds
///   [`PRUNE_MIN_RESULT_CHARS`] are replaced, by a one-line placeholder
///   recording the original size. `tool_use_id`/`is_error` stay, so every
///   tool_use keeps its paired result (the N:N contract providers enforce)
///   and the model still knows the call happened — it just has to re-run
///   the tool if it needs the details back;
/// - an attached image payload drops with the text: images are the most
///   expensive per-token content in the window;
/// - `todo` tool results are exempt — the rendered checklist is the model's
///   only live view of its plan (the tool has no read mode) and is already
///   structurally bounded (~10.4k chars worst case);
/// - pruning rewrites history, so the provider prompt-cache prefix breaks.
///   That cost is accepted because a prune only fires under context
///   pressure, where fitting the window outranks cache savings.
///
/// Returns `(rewritten history, indices of messages that lost content, chars
/// freed)`, or `None` when the pass freed less than [`PRUNE_MINIMUM_CHARS`] —
/// callers then fall through to the LLM summary path unchanged. The indices
/// (not just a count) let the caller archive the ORIGINAL messages before
/// replacing them.
pub(crate) fn prune_tool_results(
    messages: &[Message],
    protect_recent: usize,
) -> Option<(Vec<Message>, Vec<usize>, usize)> {
    let n = messages.len();
    // Short histories: the protection window covers everything.
    // saturating_sub mirrors find_safe_cut's pathological-keep_recent guard.
    if n <= protect_recent {
        return None;
    }
    // tool_use ids whose results must survive verbatim (todo = plan view).
    let exempt = todo_use_ids(messages);
    let mut freed = 0usize;
    let mut touched: Vec<usize> = Vec::new();
    let mut out = Vec::with_capacity(n);
    for (i, m) in messages.iter().enumerate() {
        // The original-task head and the protected recent window pass
        // through untouched.
        if i == 0 || i + protect_recent >= n {
            out.push(m.clone());
            continue;
        }
        let mut m = m.clone();
        let mut m_touched = false;
        for b in &mut m.content {
            let Block::ToolResult {
                tool_use_id,
                content,
                image,
                ..
            } = b
            else {
                continue;
            };
            if content.len() <= PRUNE_MIN_RESULT_CHARS || exempt.contains(&tool_use_id.as_str()) {
                continue;
            }
            let img = image.as_deref().map(image_payload_len).unwrap_or(0);
            let placeholder = format!("[pruned tool output, was {} chars]", content.len());
            // saturating on both terms, per the whole-codebase numeric
            // discipline (a hostile/huge tool result must not wrap `freed`).
            freed = freed
                .saturating_add(content.len().saturating_sub(placeholder.len()))
                .saturating_add(img);
            *content = placeholder;
            *image = None;
            m_touched = true;
        }
        if m_touched {
            touched.push(i);
        }
        out.push(m);
    }
    (freed >= PRUNE_MINIMUM_CHARS).then_some((out, touched, freed))
}

/// Size an image payload contributes to the context — `data.len() / 100`,
/// EXACTLY matching how [`total_chars`] counts images. The two must share one
/// yardstick: `freed` feeds the same "did we shrink enough?" decisions the
/// threshold gate makes with `total_chars`, so the old raw-`data.len()` count
/// overstated an image's relief by ~100× — a 200 KB payload "freed" 200k
/// chars while the window only got ~2k smaller, so prune declared victory,
/// skipped the LLM summary, and the next turn re-triggered compression (and
/// re-broke the prompt-cache prefix) on the still-oversized history.
/// `context_report` also reads it, to split a ToolResult's image payload
/// into the image bucket while its text goes to the result bucket.
pub(crate) fn image_payload_len(b: &Block) -> usize {
    match b {
        Block::Image { data, .. } => data.len() / 100,
        _ => 0,
    }
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
    // get (not `messages[..cut]`): same pathological-cut defense as
    // fold_summary — a cut past the end summarizes an empty window instead
    // of panicking. window_lines feeds the summary prompt; an empty
    // transcript there degrades the summary, never the run.
    for (i, m) in messages.get(..cut).unwrap_or_default().iter().enumerate() {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        for b in &m.content {
            // Skip the head's system-managed blocks at i==0 — the running
            // summary is the thing being UPDATED, and the todo-checklist /
            // verbatim-user-quote blocks are already preserved structurally
            // in the head; re-feeding any of them as transcript would
            // duplicate, and the quotes would invite paraphrasing what is
            // deliberately kept verbatim.
            if i == 0 {
                if let Block::Text(t) = b {
                    if t.starts_with(SUMMARY_MARKER)
                        || t.starts_with(TODO_MARKER)
                        || t.starts_with(USER_QUOTE_MARKER)
                        // A head note (turn-1 injection merges into the task
                        // message) is already preserved verbatim in the head
                        // — re-feeding it as transcript would duplicate it.
                        || t.starts_with(SUPERVISOR_NOTE_MARKER)
                    {
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
    let base = match old_summary {
        Some(old) if !old.trim().is_empty() => format!(
            "Below is the current running summary of the earlier conversation, followed by newer \
             conversation. Produce an UPDATED running summary that folds in the new material while \
             keeping earlier facts that are still relevant.\n\n\
             ## Current running summary\n{old}\n\n## Newer conversation\n{body}"
        ),
        _ => format!("Summarize this conversation transcript:\n\n{body}"),
    };
    // Supervisor-note guidance rides every summary call (R1): notes are
    // time-sensitive control signals from an external observer — the agent
    // must not repeat a mistake the observer already flagged. Their gist
    // belongs in the summary; verbatim preservation is deliberately NOT
    // wanted (they go stale), so this is the only preservation channel.
    // Appended here (not baked into SUMMARY_SYSTEM) so the marker stays
    // single-sourced from the constant.
    format!(
        "{base}\n\n\
         ## Supervisor notes\n\
         User text starting with `{SUPERVISOR_NOTE_MARKER}` is a note injected by an external \
         supervisor mid-run. Keep the gist of the RECENT ones in the summary — dropping one lets \
         the agent repeat a mistake that was already flagged — but do not preserve them verbatim; \
         they are time-sensitive and stale once acted on."
    )
}

/// Append the host's focus instructions (manual `compact_with`) to a summary
/// prompt. `None`/blank passes the prompt through unchanged. Appended to the
/// USER prompt rather than SUMMARY_SYSTEM on purpose: the system prompt is
/// shared by every summary call of every compaction (and is cached provider-
/// side), while the hint belongs to this one fold only.
pub(crate) fn with_focus_instructions(prompt: &str, instructions: Option<&str>) -> String {
    match instructions {
        Some(i) if !i.trim().is_empty() => format!(
            "{prompt}\n\n## Additional instructions from the host\n\
             When writing the summary, pay special attention to the following:\n{i}"
        ),
        _ => prompt.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Role;

    fn user_text(text: &str) -> Message {
        Message {
            id: None,
            role: Role::User,
            content: vec![Block::Text(text.to_string())],
        }
    }

    /// The regression this weighting exists for: 1000 Han characters are 3000
    /// UTF-8 bytes (750 tokens under the old bytes/4 rule) but ~1500–2500 real
    /// tokens. The CJK-aware estimate must land in that band, not at 750.
    #[test]
    fn cjk_text_is_not_under_counted_by_utf8_bytes() {
        let text = "汉".repeat(1_000);
        let est = estimate_tokens(&[user_text(&text)], None, 0);
        assert_eq!(est, 2_250, "9 units/char ÷ 4 units/token");
        assert!(est > text.len() / CHARS_PER_TOKEN * 2);
    }

    /// CJK ranges (Han, kana, Hangul, fullwidth forms) weigh 9 units; other
    /// non-ASCII code points weigh 4 (one token each); ASCII stays 1.
    #[test]
    fn char_units_cover_cjk_and_other_scripts() {
        for c in ['汉', 'あ', 'ア', '가', 'Ａ', '、', '。'] {
            assert_eq!(char_units(c), CJK_UNITS, "{c:?}");
        }
        for c in ['é', 'Ж', 'م', '😀'] {
            assert_eq!(char_units(c), OTHER_UNITS, "{c:?}");
        }
        assert_eq!(char_units('a'), 1);
        assert_eq!(text_units("a汉b"), 1 + CJK_UNITS + 1);
    }

    /// English and ASCII tool schemas keep the historical numbers — the
    /// change is additive for CJK, not a global shift that would move every
    /// preflight threshold.
    #[test]
    fn ascii_estimate_is_unchanged() {
        let text = "hello world";
        assert_eq!(
            estimate_tokens(&[user_text(text)], None, 0),
            text.len() / CHARS_PER_TOKEN
        );
        assert_eq!(
            estimate_tokens(&[], Some(text), text.len()),
            2 * text.len() / CHARS_PER_TOKEN
        );
    }

    /// The floor happens once on the whole SUM, not per block/segment.
    #[test]
    fn units_floor_once_on_the_total() {
        // "a汉b" = 11 units; 11/4 = 2 (per-segment division would give 0+2+0).
        assert_eq!(estimate_tokens(&[user_text("a汉b")], None, 0), 2);
    }
}
