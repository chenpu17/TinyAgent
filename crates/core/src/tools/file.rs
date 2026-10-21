use super::{resolve, truncate, Tool, ToolCtx, ToolOrigin, ToolOutput};
use crate::provider::ToolDef;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::Read;

/// Per-directory entry cap for listings (list_dir and list_skill) — a
/// runaway generated directory must not balloon the listing past the output
/// budget; instead the listing gets an explicit truncation marker.
pub(crate) const MAX_LIST_ENTRIES: usize = 500;
/// edit_file reads the whole file into memory (replace semantics), unlike
/// read_file which streams. Refuse a file beyond this cap rather than OOM on
/// a multi-GB file (e.g. a log in the workdir). Module-level so a test can
/// reference the real value instead of a drift-prone literal.
const EDIT_FILE_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Upper bound on a replace_all RESULT: the input is capped at EDIT_FILE_MAX_BYTES,
/// but many short matches expanded into a large new_string can otherwise produce a
/// multi-GB string and OOM before it ever hits disk. The estimate is checked BEFORE
/// the allocation.
const EDIT_FILE_OUTPUT_MAX: u128 = 64 * 1024 * 1024;
/// Bound for the whole-file totals scan behind read_file's head/JSON metadata:
/// counting every line of an arbitrarily large file on every read would make
/// paging a 1 GB log O(filesize) per call. Past this bound the totals report as
/// `N+ lines · >8 MB` — a lower bound that still answers the arm-decision
/// question ("this will not fit in the conversation") without the scan cost.
const TOTALS_SCAN_MAX: u64 = 8 * 1024 * 1024;
/// Cap for one raw (base64) read window. Base64 expands 3→4 and the guest's
/// data-scale budget is 2M chars, so 1.4 MB of raw bytes keeps the encoded
/// string under it; the effective per-call cap additionally folds in
/// ctx.max_output_chars/2 so a MAIN-loop caller (display-scale budget) cannot
/// inject a megabyte-scale blob into the conversation via raw mode.
const RAW_READ_MAX_BYTES: u64 = 1_400_000;

/// Whole-file line/char totals for read_file's size metadata (head line and
/// JSON fields). `capped` marks a scan that stopped at TOTALS_SCAN_MAX — the
/// counts are then lower bounds. `chars` counts UTF-8 non-continuation bytes,
/// which is exact for valid UTF-8 and a tight approximation otherwise.
struct Totals {
    lines: u64,
    chars: u64,
    capped: bool,
}

fn scan_totals(full: &std::path::Path) -> std::io::Result<Totals> {
    use std::io::BufRead;
    let mut r = std::io::BufReader::new(std::fs::File::open(full)?);
    let mut nl = 0u64;
    let mut chars = 0u64;
    let mut last: Option<u8> = None;
    let mut capped = false;
    let mut scanned = 0u64;
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        let mut take = chunk.len();
        if scanned.saturating_add(take as u64) > TOTALS_SCAN_MAX {
            take = (TOTALS_SCAN_MAX - scanned) as usize; // >0: scanned < MAX here
            capped = true;
        }
        let c = &chunk[..take];
        nl += c.iter().filter(|&&b| b == b'\n').count() as u64;
        chars += c.iter().filter(|&&b| (b & 0xC0) != 0x80).count() as u64;
        last = c.last().copied();
        let len = take;
        r.consume(len);
        scanned += len as u64;
        if capped {
            break;
        }
    }
    let lines = nl + u64::from(last.is_some_and(|b| b != b'\n'));
    Ok(Totals {
        lines,
        chars,
        capped,
    })
}

/// Did the window get cut mid-character by the byte cap even though the
/// OPEN-TIME stat said it wouldn't be? `file_len` — and therefore
/// `byte_capped` in the read loop — is stat'd BEFORE the open, so a file
/// that GROWS past the cap while the window streams leaves byte_capped=false
/// while `take` still cut the last line mid-character, and the decode used
/// to land in the hard "file is not valid UTF-8" error for a perfectly
/// legal file. The cut is detectable from the decode itself:
/// `error_len().is_none()` (an INCOMPLETE trailing sequence — garbage in
/// the middle reports `Some`) plus the window having actually filled the
/// cap (`window_bytes == max_bytes`; `take` never over-delivers, so a
/// not-yet-filled window ending mid-character can only be genuinely
/// invalid input). Edge knowingly accepted: a file whose length from the
/// window start EQUALS the cap and ends mid-character is indistinguishable
/// from a grown cut and now gets the valid-prefix treatment too — the same
/// prefix-not-refuse policy the byte_capped arm already applies.
/// Cap-cut predicate, shared with skill.rs's `read_capped_utf8`: it reads
/// under the same stat-then-take shape and needs the same stale-metadata
/// ruling (see its call site for the full story).
pub(crate) fn grew_past_cap(e: &std::str::Utf8Error, window_bytes: u64, max_bytes: u64) -> bool {
    e.error_len().is_none() && window_bytes >= max_bytes
}

/// read_file's line offset: 1-based, default 1, floored at 1. Converted via
/// `try_from`, never `as usize`: on a 32-bit target an offset of 2^32
/// TRUNCATES to 0 under `as` — and the `.max(1)` floor already ran on the
/// u64, so the truncated 0 stays 0 and the window silently restarts at the
/// file head. An offset that cannot fit this platform's usize is a
/// parameter error instead.
fn parse_offset(input: &Value) -> Result<usize> {
    let raw = input["offset"].as_u64().unwrap_or(1).max(1);
    usize::try_from(raw)
        .map_err(|_| anyhow::anyhow!("read_file: offset {raw} does not fit this platform's usize"))
}

/// read_file's optional line limit, through the same `try_from` rule as
/// [`parse_offset`]: on a 32-bit target `as usize` would truncate 2^32 to 0,
/// and a limit of 0 makes the "line limit reached" cut fire on the very
/// first line. (The raw-mode window is not affected — its limit is bounded
/// by `min(cap)` before any conversion.)
fn parse_limit(input: &Value) -> Result<Option<usize>> {
    match input["limit"].as_u64() {
        Some(n) => usize::try_from(n).map(Some).map_err(|_| {
            anyhow::anyhow!("read_file: limit {n} does not fit this platform's usize")
        }),
        None => Ok(None),
    }
}

pub struct ReadFile;

impl Tool for ReadFile {
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }

    /// Guest-registry opt-in (run_code's `tools.call` surface) — the file
    /// tools are the sandbox's built-in I/O substrate.
    fn available_to_code(&self) -> bool {
        true
    }

    /// Every `read_file` input is a pure read (LD-19's v1 batch whitelist;
    /// the write/list siblings deliberately stay out — list is not on the
    /// whitelist v1 and the writers are side effects by definition).
    fn batchable(&self) -> bool {
        true
    }

    fn def(&self) -> ToolDef {
        ToolDef {
            name: "read_file".into(),
            description: "Read a text file with line numbers (path relative to the working \
                          directory). The first output line reports the file's total size \
                          (`[name: N lines · M chars · complete]`, or `· lines a-b` when the \
                          window is partial) — use it to decide up front whether the data \
                          fits in the conversation or should be processed programmatically. \
                          `format:\"json\"` returns structured fields (content without line \
                          numbers, lines_total, chars_total, truncated, next_offset) for \
                          programmatic paging. `raw:true` reads a byte window (offset/limit \
                          in BYTES) as base64 `{content_b64, bytes_total, truncated, \
                          next_offset}` for byte-exact work (mainly inside run_code); the \
                          default window is 3-byte aligned, so a pager following next_offset \
                          can concatenate content_b64 across windows losslessly."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "description": "First line to read (1-based, optional; byte offset when raw)"},
                    "limit": {"type": "integer", "description": "Max number of lines (bytes when raw, optional)"},
                    "format": {"type": "string", "enum": ["json"], "description": "Return structured JSON instead of numbered text (optional)"},
                    "raw": {"type": "boolean", "description": "Read a byte window as base64 (implies JSON output; offset/limit in bytes, optional)"}
                },
                "required": ["path"]
            }),
        }
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path = input["path"]
            .as_str()
            .context("read_file: missing \"path\"")?;
        let full = resolve(ctx, path)?;
        let file_len = std::fs::metadata(&full).map(|m| m.len()).unwrap_or(0);
        if input["raw"].as_bool().unwrap_or(false) {
            return read_file_raw(path, &full, input, ctx, file_len);
        }
        let json_mode = input["format"].as_str() == Some("json");

        // Stream line by line and stop once the output cap is hit, so a
        // huge file never gets pulled into memory whole.
        let f = std::fs::File::open(&full)
            .with_context(|| format!("cannot read {}", full.display()))?;
        let mut reader = std::io::BufReader::new(f);
        // Bound the BYTES read for the OUTPUT WINDOW so a newline-less huge
        // file can't OOM on a single line allocation — read_until buffers a
        // whole line into memory before the per-line truncation below can
        // throw it away. 4× the char cap + slack mirrors the shell tool's
        // byte budget: ample for the window, and no single line can exceed
        // it. The cap is applied AFTER the offset skip (see below), not from
        // the file start.
        let max_bytes = (ctx.max_output_chars.saturating_mul(4)).saturating_add(1024) as u64;

        let offset = parse_offset(input)?;
        let limit = parse_limit(input)?;

        use std::io::BufRead;

        // ── Phase 1: skip to the output window's first line ─────────────────
        // Skipping used to consume the byte cap (the old code read-and-
        // discarded offset lines under the same `take(max_bytes)`), which
        // made every page past the first ~4×max_output_chars bytes of a file
        // UNREACHABLE — real-journey A/B found a 900-line file unpageable
        // past line 573, with the boundary line silently cut mid-content.
        // Scan for newlines in fixed-size chunks instead: bounded memory,
        // and skipped lines no longer spend the window's byte budget.
        let mut window_start: u64 = 0; // bytes consumed by the skip
        let mut eof_in_skip = false;
        if offset > 1 {
            let need = offset - 1;
            let mut skipped = 0usize;
            let mut total_nl = 0usize; // every newline seen while skipping
            let mut last_ended_nl = true; // ...and whether the stream so far ends with one
            loop {
                let chunk = match reader.fill_buf() {
                    Ok(b) => b,
                    Err(e) => {
                        return Err(e).with_context(|| format!("cannot read {}", full.display()))
                    }
                };
                if chunk.is_empty() {
                    // EOF before the skip finished: `need` overshoots the file.
                    eof_in_skip = true;
                    break;
                }
                let nl = chunk.iter().filter(|&&b| b == b'\n').count();
                if skipped + nl >= need {
                    // The need-th newline of this chunk ends the skip.
                    let want = need - skipped;
                    let mut cnt = 0usize;
                    let mut idx = chunk.len();
                    for (i, &b) in chunk.iter().enumerate() {
                        if b == b'\n' {
                            cnt += 1;
                            if cnt == want {
                                idx = i + 1;
                                break;
                            }
                        }
                    }
                    reader.consume(idx);
                    window_start += idx as u64;
                    break;
                }
                let len = chunk.len();
                last_ended_nl = chunk.ends_with(b"\n");
                total_nl += nl;
                reader.consume(len);
                window_start += len as u64;
                skipped += nl;
            }
            if eof_in_skip {
                if json_mode {
                    let t = scan_totals(&full).ok();
                    return Ok(ToolOutput::text(read_file_json(
                        "",
                        offset,
                        0,
                        t.as_ref(),
                        false,
                        "none",
                        None,
                    )));
                }
                let total_lines = total_nl + usize::from(!last_ended_nl);
                return Ok(ToolOutput::text(format!(
                    "... [offset {offset} beyond EOF; file has {total_lines} lines]\n"
                )));
            }
        }

        // ── Phase 2: read the window under the byte + char caps ────────────
        let mut reader = reader.take(max_bytes);
        // Does the byte cap actually bite? Computed against the REMAINING
        // file from the window start: the manual line loop below uses this
        // to deliver the valid prefix of a fragment the cap split mid-
        // multi-byte-char (BufRead::lines() turned that into a hard "invalid
        // UTF-8" error for ANY CJK window larger than the cap).
        let byte_capped = file_len.saturating_sub(window_start) > max_bytes;
        // C14: the output budget is tracked in CHARS (matching
        // max_output_chars and truncate()). `out.len()` is bytes, so CJK
        // content hit the budget at ~⅓ of the configured chars and truncated
        // early. `formatted` includes the line number + tab prefix, all
        // single-byte-ish overhead — counting the whole formatted line keeps
        // the accounting simple and the cap conservative.
        let mut out = String::new(); // text mode accumulator
        let mut plain = String::new(); // json mode content accumulator
        let mut lines_seen = 0usize;
        let mut emitted = 0usize; // lines actually delivered to the output
        let mut chars_out = 0usize;
        let mut window_bytes = 0u64;
        let mut truncation = "none";
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) => break, // clean EOF
                Ok(_) => {}
                Err(e) => return Err(e).with_context(|| format!("cannot read {}", full.display())),
            }
            window_bytes += buf.len() as u64;
            // (A byte cap that cuts a line mid-content always coincides with
            // the CHAR cap tripping on the same line — 4× byte budget ≥ any
            // line's chars at that point — so `truncate()`'s marker already
            // marks every real cut. The OLD from-file-start accounting could
            // deliver a silently cut SHORT boundary line; windowing removed
            // that shape.)
            let i = lines_seen; // 0-based line index within the window
            lines_seen += 1;
            // JSON mode preserves each line's ORIGINAL terminator (incl. CRLF
            // and the file's final newline): `content` is the data channel —
            // readAll/readLines and newline-counting consumers must see the
            // exact bytes, not a normalized "lines joined by \n" rendering
            // (which silently dropped every page's trailing newline — an A/B
            // task found both arms undercounting by exactly one per file).
            // Text mode keeps stripping for display.
            let term: &str = if buf.ends_with(b"\r\n") {
                "\r\n"
            } else if buf.ends_with(b"\n") {
                "\n"
            } else {
                "" // EOF without a final newline (or a mid-line byte-cap cut)
            };
            if let Some(l) = limit {
                if i >= l {
                    out.push_str("... [line limit reached]\n");
                    truncation = "limit";
                    break;
                }
            }
            // Strip EXACTLY the terminator `term` identified above — not every
            // trailing CR/LF. trim_end_matches(['\r','\n']) ate content chars:
            // "a\r\r\n" lost the \r before the CRLF and an EOF "b\r" lost its
            // trailing \r, so json-mode readAll() silently dropped bytes while
            // reporting truncated=false (external re-review P2).
            let strip_term = |s: &str| -> String {
                let stripped = if term == "\r\n" {
                    s.strip_suffix("\r\n").unwrap_or(s)
                } else if term == "\n" {
                    s.strip_suffix('\n').unwrap_or(s)
                } else {
                    s
                };
                stripped.to_string()
            };
            let mut line = match std::str::from_utf8(&buf) {
                Ok(s) => strip_term(s),
                // The cap split a multi-byte char: deliver the valid prefix
                // (valid_up_to is a char boundary by definition) instead of
                // failing the read. Two triggers: `byte_capped` (the
                // pre-open stat already promised the cut) or a file that
                // GREW past the cap after that stat — see grew_past_cap.
                Err(e) if byte_capped || grew_past_cap(&e, window_bytes, max_bytes) => {
                    strip_term(&String::from_utf8_lossy(&buf[..e.valid_up_to()]))
                }
                // Strict UTF-8 for a genuinely invalid file under the cap is
                // a real error — not silent lossy replacement.
                Err(_) => {
                    return Err(anyhow::anyhow!(
                        "cannot read {}: file is not valid UTF-8",
                        full.display()
                    ))
                }
            };
            // Cap a pathological single line BEFORE formatting+pushing: a
            // multi-MB line would otherwise be allocated into `out` in full
            // (far past max_output_chars) only for the truncation below to
            // throw it away. Char-based gate; truncate() is char-boundary-safe.
            let remaining = ctx.max_output_chars.saturating_sub(chars_out);
            if line.chars().count() > remaining {
                truncate(&mut line, remaining);
            }
            if json_mode {
                plain.push_str(&line);
                plain.push_str(term);
                chars_out += line.chars().count() + term.chars().count();
            } else {
                let formatted = format!("{:>6}\t{}\n", offset + i, line);
                chars_out += formatted.chars().count();
                out.push_str(&formatted);
            }
            emitted += 1;
            if chars_out >= ctx.max_output_chars {
                if !json_mode {
                    truncate(&mut out, ctx.max_output_chars);
                }
                // The char cap can cut mid-line; in JSON mode content's last
                // line may be partial and line-aligned resume is impossible
                // (a paged reader must switch to raw mode). In text mode the
                // truncate() marker already says so — both need the head's
                // range (not "complete").
                truncation = "chars";
                break;
            }
        }
        // Did the byte cap end the window (line-aligned or mid-line) without
        // limit/chars already claiming the reason?
        if truncation == "none" && file_len > window_start.saturating_add(window_bytes) {
            truncation = "bytes";
        }
        let truncated = truncation != "none";
        if json_mode {
            let t = scan_totals(&full).ok();
            // Line-aligned resume is only exact under the limit cut; chars/
            // bytes cuts can land mid-line.
            let next = match truncation {
                "limit" => Some((offset + emitted) as u64),
                _ => None,
            };
            return Ok(ToolOutput::text(read_file_json(
                &plain,
                offset,
                emitted,
                t.as_ref(),
                truncated,
                truncation,
                next,
            )));
        }
        if out.is_empty() {
            // The skip landed exactly on EOF (the file ends with the last
            // skipped newline) or the file is empty — both mean the window
            // is past the end. Report the true line count rather than the
            // old "[empty file]" misdirection.
            if offset == 1 {
                out.push_str("[empty file]");
                return Ok(ToolOutput::text(out));
            }
            out.push_str(&format!(
                "... [offset {offset} beyond EOF; file has {} lines]\n",
                offset - 1
            ));
            return Ok(ToolOutput::text(out));
        }
        // ── Head metadata line: the file's totals up front ─────────────────
        // The arm-decision information ("does this fit in the conversation?")
        // must be available at the FIRST read, before paging has sunk any
        // cost — the model can then pick the right strategy immediately.
        // `complete` vs a `lines a-b` range doubles as the completeness
        // signal for text mode.
        let t = scan_totals(&full).ok();
        let size_part = match &t {
            Some(t) if t.capped => format!("{}+ lines · >{} MB", t.lines, TOTALS_SCAN_MAX / MIB),
            Some(t) => format!("{} lines · {} chars", t.lines, t.chars),
            None => ">8 MB".to_string(),
        };
        let range_part = if truncated {
            format!("lines {}-{}", offset, offset + emitted.saturating_sub(1))
        } else {
            "complete".to_string()
        };
        let head = format!("[{path}: {size_part} · {range_part}]\n");
        Ok(ToolOutput::text(format!("{head}{out}")))
    }
}

const MIB: u64 = 1024 * 1024;

/// raw:true — a byte window as base64. offset/limit are BYTES here (a
/// line-indexed offset cannot address byte-exact slices). The limit folds in
/// half the caller's char budget so a display-scale caller can't pull a
/// data-scale blob into the conversation, while the guest's 2M-char budget
/// still gets the full RAW_READ_MAX_BYTES window.
fn read_file_raw(
    path: &str,
    full: &std::path::Path,
    input: &Value,
    ctx: &ToolCtx,
    file_len: u64,
) -> Result<ToolOutput> {
    use base64::Engine;
    use std::io::{Read, Seek, SeekFrom};
    let offset = input["offset"].as_u64().unwrap_or(0);
    let cap = RAW_READ_MAX_BYTES
        .min((ctx.max_output_chars as u64) / 2)
        .max(1);
    // An EXPLICIT limit is honored as-is (min the cap) — the caller asked for
    // those exact bytes. The DEFAULT window is aligned down to a multiple of
    // 3 bytes (when ≥3): base64 concatenation is lossless only at 3-byte
    // boundaries (b64(x||y) == b64(x)+b64(y) iff len(x)%3==0), so a pager
    // following next_offset without naming a limit can join content_b64
    // across windows and hand the joined string straight to write_file's
    // content_b64. The FINAL window is the short remainder — nothing follows
    // it, so any length is fine there.
    let limit = match input["limit"].as_u64() {
        Some(l) => l.min(cap),
        None if cap >= 3 => cap - cap % 3,
        None => cap,
    } as usize;
    let mut f =
        std::fs::File::open(full).with_context(|| format!("cannot read {}", full.display()))?;
    if offset >= file_len {
        return Ok(ToolOutput::text(
            json!({
                "path": path, "content_b64": "", "offset": offset,
                "bytes_read": 0, "bytes_total": file_len,
                "truncated": false, "next_offset": null,
            })
            .to_string(),
        ));
    }
    f.seek(SeekFrom::Start(offset))
        .with_context(|| format!("cannot read {}", full.display()))?;
    let mut bytes = vec![0u8; limit];
    let n = f
        .read(&mut bytes)
        .with_context(|| format!("cannot read {}", full.display()))?;
    bytes.truncate(n);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let end = offset + n as u64;
    let truncated = end < file_len;
    Ok(ToolOutput::text(
        json!({
            "path": path, "content_b64": b64, "offset": offset,
            "bytes_read": n, "bytes_total": file_len,
            "truncated": truncated,
            "next_offset": if truncated { json!(end) } else { Value::Null },
        })
        .to_string(),
    ))
}

/// format:"json" — the same line window as text mode, but structured: content
/// carries no line-number prefixes, and totals/truncation/next_offset let a
/// paged reader loop without parsing display markers.
#[allow(clippy::too_many_arguments)]
fn read_file_json(
    content: &str,
    first_line: usize,
    lines: usize,
    totals: Option<&Totals>,
    truncated: bool,
    truncation: &str,
    next_offset: Option<u64>,
) -> String {
    json!({
        "content": content,
        "first_line": first_line,
        "lines": lines,
        "lines_total": totals.map(|t| t.lines),
        "chars_total": totals.map(|t| t.chars),
        "totals_capped": totals.map(|t| t.capped),
        "truncated": truncated,
        "truncation": truncation,
        "next_offset": next_offset,
    })
    .to_string()
}

pub struct WriteFile;

impl Tool for WriteFile {
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }

    /// Guest-registry opt-in (run_code's `tools.call` surface) — the file
    /// tools are the sandbox's built-in I/O substrate.
    fn available_to_code(&self) -> bool {
        true
    }

    fn def(&self) -> ToolDef {
        ToolDef {
            name: "write_file".into(),
            description: "Write (overwrite) a file with the given full content. \
                          Parent directories are created automatically. Pass \
                          `content_b64` (base64) instead of `content` to write \
                          exact bytes — the only byte-faithful write path when \
                          the payload is not valid text or must survive an exact \
                          round-trip."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string", "description": "Complete new file content (UTF-8 text)"},
                    "content_b64": {"type": "string", "description": "Complete content as base64 bytes (alternative to content; writes exact bytes)"}
                },
                "required": ["path"]
            }),
        }
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        use base64::Engine;
        let path = input["path"]
            .as_str()
            .context("write_file: missing \"path\"")?;
        let content = input["content"].as_str();
        let content_b64 = input["content_b64"].as_str();
        let bytes: Vec<u8> = match (content, content_b64) {
            (Some(_), Some(_)) => {
                bail!("write_file: pass either \"content\" or \"content_b64\", not both")
            }
            (Some(c), None) => c.as_bytes().to_vec(),
            (None, Some(b64)) => base64::engine::general_purpose::STANDARD
                .decode(b64)
                .context("write_file: content_b64 is not valid base64")?,
            (None, None) => bail!("write_file: missing \"content\""),
        };
        let full = resolve(ctx, path)?;
        // Atomic write (temp + fsync + rename), matching edit_file: a crash or
        // SIGKILL mid-write_file must not leave a truncated target file. The
        // previous std::fs::write overwrote in place, so a half-written file
        // survived the interruption — and once compression reclaims the
        // write_file's content from history, the model can't redo the write
        // from an in-context copy either. atomic_write also creates the parent
        // dir, so the explicit create_dir_all above is dropped.
        crate::fsutil::atomic_write(&full, &bytes)
            .with_context(|| format!("cannot write {}", full.display()))?;
        // Echo the path as given (usually relative) — less confusing than
        // the canonicalized absolute form.
        Ok(ToolOutput::text(format!(
            "wrote {path} ({} bytes)",
            bytes.len()
        )))
    }
}

pub struct EditFile;

impl Tool for EditFile {
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }

    /// Guest-registry opt-in (run_code's `tools.call` surface) — the file
    /// tools are the sandbox's built-in I/O substrate.
    fn available_to_code(&self) -> bool {
        true
    }

    fn def(&self) -> ToolDef {
        ToolDef {
            name: "edit_file".into(),
            description: "Edit a file by replacing an exact occurrence of `old_string` with \
                          `new_string`. Read the file first to copy exact text (including \
                          indentation). By default `old_string` must appear exactly once; set \
                          `replace_all: true` to replace every occurrence. To create a file or \
                          rewrite it wholesale, use write_file."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string", "description": "Exact text to find"},
                    "new_string": {"type": "string", "description": "Text to replace it with"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false; otherwise old_string must be unique)"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path = input["path"]
            .as_str()
            .context("edit_file: missing \"path\"")?;
        let old_string = input["old_string"]
            .as_str()
            .context("edit_file: missing \"old_string\"")?;
        let new_string = input["new_string"]
            .as_str()
            .context("edit_file: missing \"new_string\"")?;
        let replace_all = input["replace_all"].as_bool().unwrap_or(false);

        if old_string.is_empty() {
            bail!("edit_file: old_string is empty; use write_file to create a file");
        }
        if old_string == new_string {
            bail!("edit_file: old_string and new_string are identical (no change)");
        }

        // resolve() honors restrict_to_workdir for both the read and the write
        // (same sandbox as read_file/write_file); a path escaping the workdir is
        // rejected before any I/O.
        let full = resolve(ctx, path)?;
        // edit_file needs the whole file in memory (replace semantics), unlike
        // read_file which streams and stops at max_output_chars. Refuse a file
        // beyond the module-level cap (EDIT_FILE_MAX_BYTES) rather than OOM on
        // a multi-GB file (e.g. a log in the workdir). Read with a `take` cap
        // instead of metadata()-then-read: the metadata check had a
        // check-then-read race (a file growing between the two calls slipped
        // past the limit), the same pattern load_image's take() already avoids.
        let mut bytes = Vec::new();
        std::fs::File::open(&full)
            .with_context(|| format!("cannot read {}", full.display()))?
            .take(EDIT_FILE_MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("cannot read {}", full.display()))?;
        if bytes.len() as u64 > EDIT_FILE_MAX_BYTES {
            bail!(
                "edit_file: {} is over the {EDIT_FILE_MAX_BYTES}-byte limit (edit_file reads the whole file into memory) — use read_file to inspect it, or a streaming editor",
                full.display(),
            );
        }
        let original = String::from_utf8(bytes)
            .with_context(|| format!("{} is not valid UTF-8", full.display()))?;

        let count = original.matches(old_string).count();
        if count == 0 {
            bail!(
                "edit_file: old_string not found in {path} ({} bytes) — re-read the file for exact text",
                original.len()
            );
        }
        if count > 1 && !replace_all {
            bail!(
                "edit_file: old_string matches {count} times in {path}; include more surrounding context, or set replace_all: true"
            );
        }

        // Guard replace_all's RESULT size: original is ≤8 MiB, but many short
        // matches replaced by a large new_string can balloon the output into a
        // multi-GB allocation (and OOM) before it hits disk. Estimate without
        // allocating and bail when it would exceed the cap.
        if replace_all {
            let est = (original.len() as u128)
                .saturating_add((count as u128).saturating_mul(new_string.len() as u128))
                .saturating_sub((count as u128).saturating_mul(old_string.len() as u128));
            if est > EDIT_FILE_OUTPUT_MAX {
                bail!(
                    "edit_file: replace_all would produce ~{est} bytes ({count} × {:?} → {:?}); over the {EDIT_FILE_OUTPUT_MAX}-byte result limit — narrow old_string or edit in smaller chunks",
                    old_string,
                    new_string
                );
            }
        }
        let updated = if replace_all {
            original.replace(old_string, new_string)
        } else {
            original.replacen(old_string, new_string, 1)
        };
        // Atomic (temp + fsync + rename): unlike write_file — which the model
        // can redo from its in-context copy — edit_file mutates an existing file
        // the model may NOT have in full, so a crash mid-write (panic=abort,
        // SIGKILL, power loss) must not leave it truncated. Same helper
        // result.json/report.json use.
        crate::fsutil::atomic_write(&full, updated.as_bytes())
            .with_context(|| format!("cannot write {}", full.display()))?;
        let n = if replace_all { count } else { 1 };
        Ok(ToolOutput::text(format!(
            "edited {path} ({n} replacement{}; {} → {} bytes)",
            if n == 1 { "" } else { "s" },
            original.len(),
            updated.len()
        )))
    }
}

pub struct ListDir;

impl Tool for ListDir {
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }

    /// Guest-registry opt-in (run_code's `tools.call` surface) — the file
    /// tools are the sandbox's built-in I/O substrate.
    fn available_to_code(&self) -> bool {
        true
    }

    fn def(&self) -> ToolDef {
        ToolDef {
            name: "list_dir".into(),
            description: "List entries of a directory (path relative to the working directory)."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory path, default \".\""}
                }
            }),
        }
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path = input["path"].as_str().unwrap_or(".");
        let full = resolve(ctx, path)?;
        let mut entries: Vec<String> = Vec::new();
        let mut overflow = false;
        for e in
            std::fs::read_dir(&full).with_context(|| format!("cannot list {}", full.display()))?
        {
            let e = e?;
            let suffix = if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                "/"
            } else {
                ""
            };
            entries.push(format!("{}{}", e.file_name().to_string_lossy(), suffix));
            if entries.len() >= MAX_LIST_ENTRIES {
                overflow = true;
                break;
            }
        }
        entries.sort();
        // The overflow marker goes AFTER the sort — inside the loop it sorted
        // as "..." (byte-wise before most file names) and ended up at the TOP
        // of the listing, masquerading as a directory entry.
        if overflow {
            entries.push(format!("... [more than {MAX_LIST_ENTRIES} entries]"));
        }
        if entries.is_empty() {
            return Ok(ToolOutput::text("[empty directory]"));
        }
        let mut out = entries.join("\n");
        truncate(&mut out, ctx.max_output_chars);
        Ok(ToolOutput::text(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn ctx_in(dir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            tool_absent_notes: Vec::new(),
            workdir: dir.to_path_buf(),
            max_output_chars: 1000,
            restrict_to_workdir: false,
            cancel: None,

            shell_timeout_secs: 120,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
        }
    }

    #[test]
    fn read_file_offset_beyond_eof_reports_range() {
        let dir = std::env::temp_dir().join(format!("ta-read-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "line1\nline2\n").unwrap();
        let out = ReadFile
            .run(&json!({"path": "f.txt", "offset": 99}), &ctx_in(&dir))
            .unwrap();
        assert!(out.content.contains("beyond EOF"), "got: {}", out.content);
        assert!(out.content.contains("2 lines"), "got: {}", out.content);
        fs::remove_dir_all(&dir).ok();
    }

    /// Real-journey A/B regression: the offset skip used to consume the byte
    /// cap, so every page past the first ~4×max_output_chars BYTES of a file
    /// was physically unreachable (a 900-line file unpageable past line 573 —
    /// the model's aggregation silently undercounted). The skip now scans
    /// newlines without spending the window budget: a page deep into the
    /// file must return its lines, with the ORIGINAL global line numbers.
    #[test]
    fn read_file_pages_beyond_the_first_byte_window() {
        let dir = std::env::temp_dir().join(format!("ta-page-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // 800 lines × ~60 bytes ≈ 48KB ≫ max_bytes (4×1000+1024 = 5024):
        // line 700 sits far past the old cap's reach.
        let filler = "x".repeat(50);
        let content: String = (1..=800)
            .map(|i| format!("line-{i:04}-{filler}\n"))
            .collect();
        fs::write(dir.join("big.txt"), content).unwrap();
        let out = ReadFile
            .run(
                &json!({"path": "big.txt", "offset": 700, "limit": 3}),
                &ctx_in(&dir),
            )
            .unwrap();
        assert!(out.content.contains("line-0700-"), "got: {}", out.content);
        assert!(out.content.contains("line-0702-"), "got: {}", out.content);
        assert!(
            !out.content.contains("line-0699-"),
            "window must start at the offset"
        );
        // Global numbering, not window-relative.
        assert!(
            out.content.contains("   700\t"),
            "line numbers stay global: {}",
            out.content
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_file_truly_empty() {
        let dir = std::env::temp_dir().join(format!("ta-empty-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("empty.txt"), "").unwrap();
        let out = ReadFile
            .run(&json!({"path": "empty.txt"}), &ctx_in(&dir))
            .unwrap();
        assert_eq!(out.content, "[empty file]");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_file_caps_a_pathological_single_line() {
        // A single line longer than the whole output budget (minified blob,
        // base64) must be capped before formatting — not allocated in full
        // then truncated. The output stays within max_output_chars (1000
        // here) and carries the truncation marker.
        let dir = std::env::temp_dir().join(format!("ta-bigline-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let big = "x".repeat(50_000); // one line, no newline, 50× the budget
        fs::write(dir.join("big.txt"), &big).unwrap();
        let out = ReadFile
            .run(&json!({"path": "big.txt"}), &ctx_in(&dir))
            .unwrap();
        assert!(
            out.content.len() < 2000,
            "a pathological line must be capped, got {} bytes",
            out.content.len()
        );
        assert!(out.content.contains("truncated"), "got: {}", out.content);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_file_char_budget_counts_chars_not_bytes() {
        // C14: the output budget is in CHARS. CJK content (3 bytes/char) used
        // to hit the byte-based budget at ~⅓ of the configured chars —
        // 5000 汉字 with max 1000 must deliver ~1000 chars, not ~333.
        let dir = std::env::temp_dir().join(format!("ta-cjk-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cjk.txt"), "中".repeat(5000)).unwrap();
        let out = ReadFile
            .run(&json!({"path": "cjk.txt"}), &ctx_in(&dir))
            .unwrap();
        let zh = out.content.chars().filter(|c| *c == '中').count();
        assert!(
            zh >= 950,
            "expected ~1000 chars of content under a 1000-char budget, got {zh}"
        );
        assert!(out.content.contains("truncated"), "got: {}", out.content);
        fs::remove_dir_all(&dir).ok();
    }

    /// P2 (stale-stat misdiagnosis): `byte_capped` is computed from the
    /// PRE-OPEN stat, so a file that grows past the cap during the read
    /// leaves byte_capped=false while `take` still cuts the window
    /// mid-character — the decode used to fall into the hard "file is not
    /// valid UTF-8" error for a legal file. A concurrent append cannot be
    /// made deterministic in-process (no hook sits between the stat and the
    /// read), so this pins the equivalent minimal shape the grown file
    /// presents to the decoder: a window of EXACTLY max_bytes, ending one
    /// byte into a multi-byte char, with the stat saying "not capped"
    /// (file_len == max_bytes). Pre-fix this is the hard UTF-8 error.
    #[test]
    fn read_file_reports_cap_split_window_as_truncated_not_invalid_utf8() {
        let dir = std::env::temp_dir().join(format!("ta-grow-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // ctx_in gives max_output_chars=1000 → max_bytes = 4×1000+1024 = 5024.
        let max_bytes = 4 * 1000 + 1024;
        // One line, exactly max_bytes long, ending 2 bytes INTO a 3-byte 的:
        // incomplete trailing sequence (error_len None) with a filled window
        // — the grown-cut signature grew_past_cap detects.
        let mut bytes = vec![b'a'; max_bytes - 2];
        bytes.extend_from_slice(&"的".as_bytes()[..2]);
        assert_eq!(bytes.len(), max_bytes);
        fs::write(dir.join("grow.txt"), &bytes).unwrap();
        let out = ReadFile
            .run(&json!({"path": "grow.txt"}), &ctx_in(&dir))
            .unwrap();
        assert!(
            !out.content.contains("not valid UTF-8"),
            "legal prefix misdiagnosed: {}",
            out.content
        );
        assert!(out.content.contains("truncated"), "{}", out.content);
        fs::remove_dir_all(&dir).ok();
    }

    /// The grown-cut discriminator in isolation: only an INCOMPLETE trailing
    /// sequence on a FILLED window counts; middle garbage (error_len Some)
    /// and an incomplete tail under a not-yet-filled window stay genuine
    /// UTF-8 errors.
    #[test]
    fn grew_past_cap_distinguishes_cut_from_garbage() {
        let whole = "的".repeat(10);
        let cut = &whole.as_bytes()[..whole.len() - 1];
        let e = std::str::from_utf8(cut).unwrap_err();
        assert!(e.error_len().is_none());
        assert!(grew_past_cap(&e, cut.len() as u64, cut.len() as u64));
        // Garbage in the MIDDLE (error_len Some) is never a cap cut, even on
        // a filled window: 中 followed by two orphan 0xFF bytes (built
        // dynamically — a byte literal would trip clippy's
        // invalid-from-utf8, which is exactly the error we want here).
        let mut bad = "中".as_bytes().to_vec();
        bad.extend_from_slice(&[0xff, 0xff]);
        let e = std::str::from_utf8(&bad).unwrap_err();
        assert!(e.error_len().is_some());
        assert!(!grew_past_cap(&e, bad.len() as u64, bad.len() as u64));
        // Incomplete tail under a NOT-yet-filled window: the reader stopped
        // early, so the cap cannot be the cutter — stays a real error.
        let e = std::str::from_utf8(cut).unwrap_err();
        assert!(!grew_past_cap(&e, 5, 100));
    }

    /// P2 (32-bit offset truncation): the line offset must round-trip
    /// through `usize::try_from`, never `as usize` — on 32-bit targets 2^32
    /// truncates to 0 AFTER the `.max(1)` floor already ran on the u64,
    /// silently restarting the window at the file head. On 64-bit
    /// usize::MAX == u64::MAX, so no u64 can overflow; the 64-bit half pins
    /// the extreme value's SURVIVAL (no clamp, no truncate) and the 32-bit
    /// half pins the overflow → parameter error.
    #[test]
    fn read_file_offset_conversion_fits_or_errors_never_truncates() {
        #[cfg(target_pointer_width = "32")]
        {
            let err = parse_offset(&json!({"offset": u64::MAX})).unwrap_err();
            assert!(format!("{err:#}").contains("offset"), "{err:#}");
        }
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(
                parse_offset(&json!({"offset": u64::MAX})).unwrap(),
                usize::MAX
            );
        }
        // Default and the 1-floor are unchanged.
        assert_eq!(parse_offset(&json!({})).unwrap(), 1);
        assert_eq!(parse_offset(&json!({"offset": 0})).unwrap(), 1);
    }

    /// The optional limit converts through the same try_from rule: 32-bit
    /// halves pin overflow → parameter error (a truncated 0 would cut the
    /// very first line), 64-bit halves pin the extreme value's survival,
    /// absence stays None (no limit).
    #[test]
    fn read_file_limit_conversion_fits_or_errors_never_truncates() {
        #[cfg(target_pointer_width = "32")]
        {
            let err = parse_limit(&json!({"limit": u64::MAX})).unwrap_err();
            assert!(format!("{err:#}").contains("limit"), "{err:#}");
        }
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(
                parse_limit(&json!({"limit": u64::MAX})).unwrap(),
                Some(usize::MAX)
            );
        }
        assert_eq!(parse_limit(&json!({})).unwrap(), None);
    }

    /// End-to-end plumbing for an extreme offset (u32::MAX fits usize on
    /// every target): it must reach the skip phase intact — no
    /// truncation-to-zero restart at the file head — and report beyond-EOF.
    #[test]
    fn read_file_huge_offset_reports_beyond_eof_not_the_file_head() {
        let dir = std::env::temp_dir().join(format!("ta-hugeoff-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "line1\nline2\n").unwrap();
        let out = ReadFile
            .run(
                &json!({"path": "f.txt", "offset": u32::MAX as u64}),
                &ctx_in(&dir),
            )
            .unwrap();
        assert!(out.content.contains("beyond EOF"), "{}", out.content);
        assert!(!out.content.contains("line1"), "{}", out.content);
        fs::remove_dir_all(&dir).ok();
    }

    fn ctx_restricted(dir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            tool_absent_notes: Vec::new(),
            workdir: dir.to_path_buf(),
            max_output_chars: 1000,
            restrict_to_workdir: true,
            cancel: None,
            shell_timeout_secs: 120,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
        }
    }

    #[test]
    fn edit_file_unique_replace_succeeds() {
        let dir = std::env::temp_dir().join(format!("ta-edit-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let out = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "beta", "new_string": "BETA"}),
                &ctx_in(&dir),
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("1 replacement"), "{}", out.content);
        assert_eq!(
            fs::read_to_string(dir.join("f.txt")).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edit_file_not_found_errors() {
        let dir = std::env::temp_dir().join(format!("ta-edit-nf-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "alpha\n").unwrap();
        let err = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "zzz", "new_string": "y"}),
                &ctx_in(&dir),
            )
            .unwrap_err();
        assert!(format!("{err:#}").contains("not found"), "{err:#}");
        // File unchanged on failure.
        assert_eq!(fs::read_to_string(dir.join("f.txt")).unwrap(), "alpha\n");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edit_file_multiple_matches_errors_without_replace_all() {
        let dir = std::env::temp_dir().join(format!("ta-edit-mm-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "x x x\n").unwrap();
        let err = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "x", "new_string": "y"}),
                &ctx_in(&dir),
            )
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("3 times"), "{msg}");
        assert_eq!(fs::read_to_string(dir.join("f.txt")).unwrap(), "x x x\n");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edit_file_replace_all_replaces_every() {
        let dir = std::env::temp_dir().join(format!("ta-edit-all-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "x x x\n").unwrap();
        let out = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "x", "new_string": "y", "replace_all": true}),
                &ctx_in(&dir),
            )
            .unwrap();
        assert!(out.content.contains("3 replacements"), "{}", out.content);
        assert_eq!(fs::read_to_string(dir.join("f.txt")).unwrap(), "y y y\n");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edit_file_rejects_empty_and_identical_old_string() {
        let dir = std::env::temp_dir().join(format!("ta-edit-ei-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "hello\n").unwrap();
        let empty_err = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "", "new_string": "y"}),
                &ctx_in(&dir),
            )
            .unwrap_err();
        assert!(format!("{empty_err:#}").contains("empty"));
        let same_err = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "hello", "new_string": "hello"}),
                &ctx_in(&dir),
            )
            .unwrap_err();
        assert!(format!("{same_err:#}").contains("identical"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edit_file_rejects_escape_under_restrict() {
        let dir = std::env::temp_dir().join(format!("ta-edit-esc-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("ok.txt"), "hello").unwrap();
        // An absolute path outside the workdir must be rejected before any I/O
        // when restrict_to_workdir is on (same sandbox as write_file).
        let outside = std::env::temp_dir().join(format!("ta-edit-out-{}.txt", std::process::id()));
        fs::write(&outside, "hello").unwrap();
        let res = EditFile.run(
            &json!({"path": outside.to_string_lossy(), "old_string": "hello", "new_string": "bye"}),
            &ctx_restricted(&dir),
        );
        assert!(
            res.is_err(),
            "escape must be rejected under restrict_to_workdir"
        );
        // The outside file is untouched.
        assert_eq!(fs::read_to_string(&outside).unwrap(), "hello");
        fs::remove_dir_all(&dir).ok();
        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn edit_file_rejects_oversized_file() {
        // edit_file loads the whole file into memory (replace semantics); a
        // file over EDIT_FILE_MAX_BYTES must be rejected before read_to_string
        // (OOM guard). A sparse file via set_len reports a large len() without
        // actually allocating the bytes; the capped take() read then streams a
        // hole of zeros, so this test stays cheap either way.
        let dir = std::env::temp_dir().join(format!("ta-edit-big-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.txt");
        {
            let f = std::fs::File::create(&big).unwrap();
            f.set_len(EDIT_FILE_MAX_BYTES + 1).unwrap(); // sparse hole, not 8 MB of data
        }
        let err = EditFile
            .run(
                &json!({"path": "big.txt", "old_string": "x", "new_string": "y"}),
                &ctx_in(&dir),
            )
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("over the"), "{msg}");
        assert!(msg.contains("limit"), "{msg}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_dir_overflow_marker_sorts_last() {
        // Over MAX_LIST_ENTRIES the listing carries a "... [more than N
        // entries]" marker. It must sit at the END of the sorted output —
        // pushed before the sort it byte-sorts as "..." ahead of most file
        // names and ends up at the top, masquerading as a directory entry.
        let dir = std::env::temp_dir().join(format!("ta-list-big-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for i in 0..MAX_LIST_ENTRIES + 5 {
            fs::write(dir.join(format!("f{i:04}")), "").unwrap();
        }
        let mut ctx = ctx_in(&dir);
        ctx.max_output_chars = 1_000_000; // don't truncate the marker away
        let out = ListDir.run(&json!({}), &ctx).unwrap();
        let marker = format!("... [more than {MAX_LIST_ENTRIES} entries]");
        let tail = out.content.rsplit('\n').next().unwrap();
        assert_eq!(tail, marker, "marker must be the last line: {tail:?}");
        // And exactly MAX_LIST_ENTRIES real entries precede it.
        assert_eq!(out.content.lines().count(), MAX_LIST_ENTRIES + 1);
        fs::remove_dir_all(&dir).ok();
    }

    /// restrict_to_workdir must refuse a path that escapes via a symlink inside
    /// the workdir. The whole point of confine()'s canonicalize step (vs a plain
    /// lexical prefix check) is to resolve symlinks, yet nothing in the suite
    /// exercised it — the existing escape test uses an absolute path, which a
    /// lexical check would also catch. This pins the symlink-resolution path so
    /// a future simplification can't silently drop it.
    #[cfg(unix)]
    #[test]
    fn write_file_rejects_symlink_escape_under_restrict() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("ta-sym-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // A target OUTSIDE the workdir, and a symlink inside it pointing there.
        let outside = std::env::temp_dir().join(format!("ta-sym-out-{}.txt", std::process::id()));
        fs::write(&outside, "orig").unwrap();
        symlink(&outside, dir.join("evil")).unwrap();
        // Writing through the symlink must be rejected before any I/O, and the
        // outside file must stay untouched.
        let res = WriteFile.run(
            &json!({"path": "evil", "content": "hacked"}),
            &ctx_restricted(&dir),
        );
        assert!(
            res.is_err(),
            "symlink escape must be rejected under restrict_to_workdir"
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), "orig");
        fs::remove_dir_all(&dir).ok();
        let _ = fs::remove_file(&outside);
    }

    // ── v2 guest I/O: head metadata, structured JSON, raw byte windows ──────

    #[test]
    fn read_file_head_reports_totals_and_completeness() {
        // First output line = the file's totals + completeness, so the
        // "does this fit in the conversation" decision is available at the
        // FIRST read (before paging has sunk any cost).
        let dir = std::env::temp_dir().join(format!("ta-head-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "alpha\nbeta\n").unwrap();
        let out = ReadFile
            .run(&json!({"path": "f.txt"}), &ctx_in(&dir))
            .unwrap();
        let first = out.content.lines().next().unwrap();
        assert_eq!(first, "[f.txt: 2 lines · 11 chars · complete]");
        // Partial window reports the shown range instead.
        let out = ReadFile
            .run(&json!({"path": "f.txt", "limit": 1}), &ctx_in(&dir))
            .unwrap();
        let first = out.content.lines().next().unwrap();
        assert_eq!(first, "[f.txt: 2 lines · 11 chars · lines 1-1]");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_file_head_counts_missing_trailing_newline() {
        let dir = std::env::temp_dir().join(format!("ta-head-nl-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "alpha\nbeta").unwrap(); // no trailing \n
        let out = ReadFile
            .run(&json!({"path": "f.txt"}), &ctx_in(&dir))
            .unwrap();
        let first = out.content.lines().next().unwrap();
        assert_eq!(first, "[f.txt: 2 lines · 10 chars · complete]");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_file_json_mode_structures_content_and_paging() {
        // format:"json": no line-number prefixes, plus totals / truncated /
        // next_offset so a paged reader loops without parsing display markers.
        let dir = std::env::temp_dir().join(format!("ta-json-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let content: String = (1..=25).map(|i| format!("row-{i:02}\n")).collect();
        fs::write(dir.join("f.txt"), content).unwrap();
        let out = ReadFile
            .run(
                &json!({"path": "f.txt", "offset": 1, "limit": 10, "format": "json"}),
                &ctx_in(&dir),
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap();
        // Content preserves each line's terminator, including the page's
        // last newline (byte-faithful data channel — see the term logic in
        // the read loop).
        assert_eq!(
            v["content"],
            "row-01\nrow-02\nrow-03\nrow-04\nrow-05\nrow-06\nrow-07\nrow-08\nrow-09\nrow-10\n"
        );
        assert_eq!(v["first_line"], 1);
        assert_eq!(v["lines"], 10);
        assert_eq!(v["lines_total"], 25);
        assert_eq!(v["chars_total"], 175); // 25 rows × 7 chars
        assert_eq!(v["truncated"], true);
        assert_eq!(v["truncation"], "limit");
        assert_eq!(v["next_offset"], 11);
        // The reported next_offset resumes exactly where the page ended.
        let out2 = ReadFile
            .run(
                &json!({"path": "f.txt", "offset": 11, "limit": 100, "format": "json"}),
                &ctx_in(&dir),
            )
            .unwrap();
        let v2: Value = serde_json::from_str(&out2.content).unwrap();
        assert_eq!(v2["truncated"], false);
        assert_eq!(v2["next_offset"], Value::Null);
        assert!(v2["content"].as_str().unwrap().starts_with("row-11\n"));
        // Final page carries the file's closing newline too.
        assert!(v2["content"].as_str().unwrap().ends_with("row-25\n"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_file_json_mode_preserves_stray_cr_bytes() {
        // External re-review P2: trim_end_matches(['\r','\n']) ate CONTENT
        // chars — the \r before a CRLF and a trailing \r at EOF — while
        // reporting truncated=false. Only the one identified terminator may
        // be stripped per line.
        let dir = std::env::temp_dir().join(format!("ta-json-cr-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cr.txt"), "a\r\r\nb\r").unwrap();
        let out = ReadFile
            .run(&json!({"path": "cr.txt", "format": "json"}), &ctx_in(&dir))
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["content"], "a\r\r\nb\r");
        assert_eq!(v["truncated"], false);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_file_json_mode_empty_file_is_structured() {
        let dir = std::env::temp_dir().join(format!("ta-json-e-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("empty.txt"), "").unwrap();
        let out = ReadFile
            .run(
                &json!({"path": "empty.txt", "format": "json"}),
                &ctx_in(&dir),
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["content"], "");
        assert_eq!(v["lines_total"], 0);
        assert_eq!(v["truncated"], false);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_file_raw_returns_base64_byte_window() {
        // raw:true: offset/limit are BYTES; the window is exact — including
        // bytes that are not valid UTF-8 (the whole point of the mode).
        let dir = std::env::temp_dir().join(format!("ta-raw-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let bytes: Vec<u8> = (0u8..=255).collect();
        fs::write(dir.join("bin.dat"), &bytes).unwrap();
        let mut ctx = ctx_in(&dir);
        ctx.max_output_chars = 2_000_000; // guest-scale budget: full window
        let out = ReadFile
            .run(
                &json!({"path": "bin.dat", "raw": true, "offset": 3, "limit": 10}),
                &ctx,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap();
        use base64::Engine;
        assert_eq!(
            v["content_b64"],
            base64::engine::general_purpose::STANDARD.encode(&bytes[3..13])
        );
        assert_eq!(v["bytes_read"], 10);
        assert_eq!(v["bytes_total"], 256);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["next_offset"], 13);
        // Display-scale budget clamps the window instead of injecting a
        // megabyte-scale blob into the conversation. The clamped window is
        // 3-byte aligned (500 → 498) so cross-window base64 concatenation
        // stays lossless (see read_file_raw_windows_concatenate_losslessly).
        fs::write(dir.join("big.dat"), vec![0u8; 2000]).unwrap();
        let out = ReadFile
            .run(&json!({"path": "big.dat", "raw": true}), &ctx_in(&dir))
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["bytes_read"], 498, "1000-char budget → 498-byte window");
        assert_eq!(v["truncated"], true);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_file_raw_windows_concatenate_losslessly() {
        // The byte-exact copy contract (D16): a pager that follows
        // next_offset and JOINS the per-window content_b64 must get exactly
        // base64(whole file) — which requires every non-final window to be a
        // multiple of 3 bytes. RAW_READ_MAX_BYTES (1.4M) and the guest-budget
        // window (1M) are both non-multiples, so the alignment is enforced in
        // read_file_raw itself, not left to the caller (writeVerified's
        // explicit CH=1399995 limit predates this and still works).
        let dir = std::env::temp_dir().join(format!("ta-rawjoin-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut bytes = Vec::new();
        // Mixed non-UTF-8 content with a size that forces several windows
        // under a small budget (cap = max_output_chars/2 = 100 bytes →
        // 99-byte windows).
        for i in 0..350u32 {
            bytes.push((i % 256) as u8);
            bytes.push(0xFF);
        }
        fs::write(dir.join("blob.bin"), &bytes).unwrap();
        let ctx = ctx_in(&dir); // max_output_chars = 1000 → 100-byte cap
        let tool = ReadFile;
        let mut joined = String::new();
        let mut off = 0u64;
        loop {
            let out = tool
                .run(
                    &json!({"path": "blob.bin", "raw": true, "offset": off}),
                    &ctx,
                )
                .unwrap();
            let v: Value = serde_json::from_str(&out.content).unwrap();
            joined.push_str(v["content_b64"].as_str().unwrap());
            if !v["truncated"].as_bool().unwrap() {
                break;
            }
            off = v["next_offset"].as_u64().unwrap();
        }
        use base64::Engine;
        assert_eq!(
            joined,
            base64::engine::general_purpose::STANDARD.encode(&bytes),
            "joined window b64 must equal whole-file b64"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_file_b64_writes_exact_bytes() {
        // The byte-faithful path (open q1 / real-journey task4: a text
        // round-trip used to eat a trailing newline).
        let dir = std::env::temp_dir().join(format!("ta-wb64-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode("tail-no-newline");
        let out = WriteFile
            .run(
                &json!({"path": "out.txt", "content_b64": b64}),
                &ctx_in(&dir),
            )
            .unwrap();
        assert!(out.content.contains("15 bytes"), "{}", out.content);
        // Exact bytes on disk — no implicit trailing newline.
        assert_eq!(fs::read(dir.join("out.txt")).unwrap(), b"tail-no-newline");
        // Ambiguity and garbage are hard errors.
        let err = WriteFile
            .run(
                &json!({"path": "x", "content": "a", "content_b64": "YQ=="}),
                &ctx_in(&dir),
            )
            .unwrap_err();
        assert!(format!("{err:#}").contains("not both"), "{err:#}");
        let err = WriteFile
            .run(&json!({"path": "x", "content_b64": "!!!"}), &ctx_in(&dir))
            .unwrap_err();
        assert!(format!("{err:#}").contains("base64"), "{err:#}");
        fs::remove_dir_all(&dir).ok();
    }
}
