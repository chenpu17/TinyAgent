//! SSE parser review pins (external adversarial review round, provider
//! shard): the parser must be byte-invariant under every chunking a
//! hostile/slow network can produce, and the live worker path must parse
//! identically to the sync path. `sse_cr_only_line_endings_observed_behavior`
//! documents the one recorded deviation from the SSE spec (CR-only line
//! endings merge into one event) — see the SseReader docs.

use super::SseReader;
use std::io::Read;

/// A Read source that dribbles its byte stream in fixed-size chunks, so the
/// parser sees every cut point a hostile/slow network could produce.
struct Chunked {
    data: Vec<u8>,
    pos: usize,
    step: usize,
}
impl Read for Chunked {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.data.len() {
            return Ok(0);
        }
        let n = self.step.min(b.len()).min(self.data.len() - self.pos);
        b[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// An irregular chunker driven by an explicit byte-length table (cycles).
struct Irregular {
    data: Vec<u8>,
    pos: usize,
    lens: Vec<usize>,
    i: usize,
}
impl Read for Irregular {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.data.len() {
            return Ok(0);
        }
        let n = self
            .lens
            .get(self.i % self.lens.len())
            .copied()
            .unwrap_or(1)
            .max(1)
            .min(b.len())
            .min(self.data.len() - self.pos);
        self.i += 1;
        b[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn drain_all<R: Read>(r: R) -> Vec<String> {
    let mut sse = SseReader::new(r);
    let mut out = Vec::new();
    while let Some(d) = sse.next_data().unwrap() {
        out.push(d);
    }
    out
}

/// Corpus: every shape the attack list names. Each entry: (name, bytes).
fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("minimal", b"data: A\n\ndata: B\n\n".to_vec()),
        (
            "event_field_plus_crlf",
            b"event: x\r\ndata: {\"a\":1}\r\n\r\ndata: B\n\n".to_vec(),
        ),
        (
            "multiline_data_comment_between",
            b"data: line1\ndata: line2\n\n: keep-alive\ndata: C\n\n".to_vec(),
        ),
        ("done_sentinel", b"data: [DONE]\n\n".to_vec()),
        ("done_no_trailing_newline", b"data: [DONE]".to_vec()),
        ("empty_stream", Vec::new()),
        ("tail_no_newline", b"data: tail".to_vec()),
        (
            "leading_and_double_blanks",
            b"\n\ndata: X\n\n\n\ndata: Y\n\n".to_vec(),
        ),
        ("empty_payloads", b"data:\n\ndata: \n\n".to_vec()),
        (
            "bom_head",
            "\u{feff}data: X\n\ndata: Y\n\n".as_bytes().to_vec(),
        ),
        (
            "utf8_multibyte",
            "data: \"héllo 🎉 你好\"\n\ndata: ok\n\n"
                .as_bytes()
                .to_vec(),
        ),
        (
            "event_between_data_of_one_event",
            b"data: a\nevent: t\ndata: b\n\n".to_vec(),
        ),
        // Spec-legal CR-only line endings (no LF anywhere): recorded behavior.
        ("cr_only_endings", b"data: a\r\rdata: b\r\r".to_vec()),
        (
            "data_two_spaces",
            b"data:  two\n\ndata:no-space\n\n".to_vec(),
        ),
        (
            "done_then_more",
            b"data: A\n\ndata: [DONE]\n\ndata: AFTER\n\n".to_vec(),
        ),
        (
            "comment_only",
            b": ping\n\n: pong\n\ndata: after-comments\n\n".to_vec(),
        ),
        (
            "id_field_ignored",
            b"id: 42\ndata: payload\n\ndata: two\n\n".to_vec(),
        ),
        ("colon_only_line", b"data: x\n:\n\ndata: y\n\n".to_vec()),
    ]
}

#[test]
fn sse_every_fixed_step_chunking_matches_whole_stream() {
    for (name, raw) in corpus() {
        let baseline = drain_all(Chunked {
            data: raw.clone(),
            pos: 0,
            step: usize::MAX,
        });
        assert!(
            !baseline.is_empty() || raw.is_empty(),
            "{name}: baseline empty"
        );
        for step in 1..=raw.len().max(1) {
            let got = drain_all(Chunked {
                data: raw.clone(),
                pos: 0,
                step,
            });
            assert_eq!(got, baseline, "{name}: step={step} diverged");
        }
    }
}

#[test]
fn sse_irregular_chunkings_match_whole_stream() {
    let tables: Vec<Vec<usize>> = vec![
        vec![1, 2, 3, 5, 7, 11],
        vec![13, 1, 1],
        vec![2, 1, 4, 1],
        vec![3],
        vec![1],
    ];
    for (name, raw) in corpus() {
        let baseline = drain_all(Chunked {
            data: raw.clone(),
            pos: 0,
            step: usize::MAX,
        });
        for t in &tables {
            let got = drain_all(Irregular {
                data: raw.clone(),
                pos: 0,
                lens: t.clone(),
                i: 0,
            });
            assert_eq!(got, baseline, "{name}: irregular {t:?} diverged");
        }
    }
}

#[test]
fn sse_utf8_multibyte_split_at_every_byte_boundary() {
    // The payload carries 2/3/4-byte UTF-8 chars; every 1-byte dribble must
    // still decode lines whole (line buffering absorbs split chars).
    let raw = "data: aé\n\ndata: 你🎉z\n\n".as_bytes().to_vec();
    let baseline = drain_all(Chunked {
        data: raw.clone(),
        pos: 0,
        step: usize::MAX,
    });
    assert_eq!(baseline.len(), 2);
    for step in 1..=raw.len() {
        let got = drain_all(Chunked {
            data: raw.clone(),
            pos: 0,
            step,
        });
        assert_eq!(got, baseline, "utf8 split step={step}");
    }
}

#[test]
fn sse_cr_only_line_endings_observed_behavior() {
    // Recorded spec deviation (review finding, accepted): no LF anywhere
    // means the whole stream is ONE line — one merged event instead of two.
    // Real gateways emit LF or CRLF; CR-only streams are the documented
    // limitation (SseReader reads lines on LF and trims trailing CR).
    let got = drain_all(Chunked {
        data: b"data: a\r\rdata: b\r\r".to_vec(),
        pos: 0,
        step: 1,
    });
    // Observed: ONE merged event instead of two ("a", "b"); interior CRs are
    // kept as payload, only the trailing CRs trimmed.
    assert_eq!(got.len(), 1, "CR-only merges into one event: {got:?}");
    assert_eq!(got[0], "a\r\rdata: b", "interior CRs ride as payload");
}

#[test]
fn sse_cancellable_worker_path_matches_sync_path() {
    // The LIVE path (worker thread + bounded channel) must parse identically
    // to the sync SseReader for the same dribbled bytes.
    use super::CancellableSse;
    use super::SseSource;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    for (name, raw) in corpus() {
        for step in [1usize, 3, 7, usize::MAX] {
            let baseline = drain_all(Chunked {
                data: raw.clone(),
                pos: 0,
                step: usize::MAX,
            });
            let cancel = Arc::new(AtomicBool::new(false));
            let mut s = CancellableSse::new(
                Chunked {
                    data: raw.clone(),
                    pos: 0,
                    step,
                },
                cancel.clone(),
                std::time::Duration::from_secs(20),
            )
            .unwrap();
            let mut got = Vec::new();
            while let Some(d) = s.next_payload().unwrap() {
                got.push(d);
            }
            assert_eq!(got, baseline, "{name}: CancellableSse step={step} diverged");
            assert!(!cancel.load(Ordering::Relaxed));
        }
    }
}
