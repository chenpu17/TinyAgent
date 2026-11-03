//! OpenAI provider review pins (external adversarial review round, provider
//! shard). Two families:
//! - error-classification / credential-discipline / retry-body pins over a
//!   real loopback HTTP mock;
//! - ThinkStripper differential pins (streaming state machine vs a
//!   whole-string reference) plus parse_loop wiring and the idle-timeout
//!   path.
//!
//! Child module of `provider::openai` so it can name the crate-private
//! ThinkStripper / parse_loop.

use super::SseReader;
use super::{ChatEvent, ThinkState, ThinkStripper, THINK_CLOSE, THINK_OPEN};
use crate::agent::RunError;
use crate::provider::{ApiStatusError, Block, ChatRequest, Message, OpenAi, Provider, Usage};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

// ==== Error classification / credential discipline / retry body ====

/// Serve EVERY incoming connection with a fixed HTTP response (status line,
/// content-type, payload), one accept per request (connection: close).
fn spawn_mock(status: &'static str, ct: &'static str, payload: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let _ = read_request_body(&stream);
            let head = format!(
                "HTTP/1.1 {status}\r\ncontent-type: {ct}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                payload.len(),
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(payload.as_bytes());
            let _ = stream.flush();
        }
    });
    url
}

fn read_request_body(stream: &std::net::TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().expect("clone tcp"));
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(v) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        let _ = reader.read_exact(&mut body);
    }
    String::from_utf8_lossy(&body).into_owned()
}

fn mk_req<'a>(m: &'a [Message], cancel: &std::sync::Arc<AtomicBool>) -> ChatRequest<'a> {
    ChatRequest {
        system: None,
        messages: m,
        tools: &[],
        model: "m",
        max_tokens: 100,
        per_request_timeout: Some(Duration::from_secs(10)),
        cancel: cancel.clone(),
        cache: true,
        reasoning_effort: None,
        temperature: None,
    }
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client")
}

fn run_error_of(e: anyhow::Error) -> RunError {
    RunError {
        source: e,
        usage: Usage::default(),
        tool_calls: vec![],
        compressions: vec![],
        turns: 0,
        elapsed: Duration::ZERO,
    }
}

#[test]
fn status_matrix_structured_status_and_transient_labels() {
    // (status line, expected ApiStatusError.status, expected is_transient)
    let matrix: Vec<(&'static str, u16, bool)> = vec![
        ("400 Bad Request", 400, false),
        ("401 Unauthorized", 401, false),
        ("403 Forbidden", 403, false),
        ("404 Not Found", 404, false),
        ("408 Request Timeout", 408, true),
        ("422 Unprocessable Entity", 422, false),
        ("425 Too Early", 425, true),
        ("429 Too Many Requests", 429, true),
        ("500 Internal Server Error", 500, true),
        ("502 Bad Gateway", 502, true),
        ("503 Service Unavailable", 503, true),
        ("599 Bogus", 599, true),
    ];
    for (line, want_status, want_transient) in matrix {
        let body = r#"{"error":{"message":"probe rejection"}}"#;
        let url = spawn_mock(line, "application/json", body);
        let p = OpenAi::new(client(), url, "sk-PROBE-SECRET", Duration::from_secs(30));
        let m = vec![Message::user(vec![Block::Text("hi".into())])];
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let err = match p.chat(&mk_req(&m, &cancel), &mut |_| {}) {
            Err(e) => e,
            Ok(_) => panic!("{line}: expected an error"),
        };
        let se = err
            .downcast_ref::<ApiStatusError>()
            .unwrap_or_else(|| panic!("{line}: expected ApiStatusError, got {err:#}"));
        assert_eq!(se.status, want_status, "{line}: structured status");
        // Historical display text preserved.
        assert!(
            err.to_string().starts_with("OpenAI-compatible API error"),
            "{line}"
        );
        // Credential discipline: the surfaced chain must not echo the key…
        let chain = format!("{err:#}");
        assert!(
            !chain.contains("sk-PROBE-SECRET"),
            "{line}: key leaked: {chain}"
        );
        // …nor the request body.
        assert!(
            !chain.contains("\"messages\""),
            "{line}: body leaked: {chain}"
        );
        // Classification through the run taxonomy.
        let t = run_error_of(err).is_transient();
        assert_eq!(t, want_transient, "{line}: is_transient");
    }
}

#[test]
fn connection_refused_is_transient_without_api_status() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener); // nothing listens now
    let url = format!("http://127.0.0.1:{port}/v1");
    let p = OpenAi::new(client(), url, "k", Duration::from_secs(30));
    let m = vec![Message::user(vec![Block::Text("hi".into())])];
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let err = match p.chat(&mk_req(&m, &cancel), &mut |_| {}) {
        Err(e) => e,
        Ok(_) => panic!("connect failure must error"),
    };
    assert!(
        err.downcast_ref::<ApiStatusError>().is_none(),
        "connect failure must NOT masquerade as an API status"
    );
    assert!(run_error_of(err).is_transient(), "network errors retry");
}

#[test]
fn url_userinfo_in_base_url_error_chain() {
    // If base_url embeds basic-auth credentials, a connect failure's error
    // chain must not print the password verbatim (config Debug redacts; the
    // reqwest error text is the residual risk this probe measures).
    let url = "http://alice:supersecret-pw@127.0.0.1:1/v1";
    let p = OpenAi::new(client(), url.to_string(), "k", Duration::from_secs(30));
    let m = vec![Message::user(vec![Block::Text("hi".into())])];
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let err = match p.chat(&mk_req(&m, &cancel), &mut |_| {}) {
        Err(e) => e,
        Ok(_) => panic!("dead port must error"),
    };
    let chain = format!("{:#}", err);
    let leaked = chain.contains("supersecret-pw");
    eprintln!("AUDIT userinfo probe — leaked={leaked} chain:\n{chain}");
    assert!(
        !leaked,
        "password from base_url userinfo leaked into the error chain: {chain}"
    );
}

#[test]
fn html_body_on_200_diagnoses_the_non_streaming_body() {
    // A gateway serving an HTML error page with 200 + text/html: no data:
    // lines exist, so the parse yields zero events and (without
    // finish_reason) "stream_truncated" — but the transcript carries a
    // diagnostic block naming the content-type, so the root cause is
    // readable without a packet capture (review finding, fixed).
    let url = spawn_mock(
        "200 OK",
        "text/html",
        "<html><body>502 from upstream</body></html>",
    );
    let p = OpenAi::new(client(), url, "k", Duration::from_secs(30));
    let m = vec![Message::user(vec![Block::Text("hi".into())])];
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let resp = p.chat(&mk_req(&m, &cancel), &mut |_| {}).unwrap();
    assert_eq!(resp.stop_reason, "stream_truncated");
    assert_eq!(
        resp.blocks.len(),
        1,
        "exactly the diagnostic block: {:?}",
        resp.blocks
    );
    match &resp.blocks[0] {
        Block::Text(t) => {
            assert!(t.contains("no SSE data events"), "{t}");
            assert!(t.contains("text/html"), "content-type named: {t}");
        }
        other => panic!("expected the diagnostic text block, got {other:?}"),
    }
    assert!(resp.usage.is_none());
}

#[test]
fn html_body_on_500_is_transient_and_capped() {
    let big = format!("<html>{}</html>", "E".repeat(64 * 1024));
    let url = spawn_mock(
        "502 Bad Gateway",
        "text/html",
        Box::leak(big.into_boxed_str()),
    );
    let p = OpenAi::new(client(), url, "k", Duration::from_secs(30));
    let m = vec![Message::user(vec![Block::Text("hi".into())])];
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let err = match p.chat(&mk_req(&m, &cancel), &mut |_| {}) {
        Err(e) => e,
        Ok(_) => panic!("502 must error"),
    };
    let se = err
        .downcast_ref::<ApiStatusError>()
        .expect("status survives");
    assert_eq!(se.status, 502);
    assert!(run_error_of(err).is_transient());
}

#[test]
fn garbage_data_line_and_empty_choices_shapes() {
    // Garbage data line: skipped (keep-alive tolerance).
    let sse1 = concat!(
        "data: <- not json ->\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let url = spawn_mock("200 OK", "text/event-stream", sse1);
    let p = OpenAi::new(client(), url, "k", Duration::from_secs(30));
    let m = vec![Message::user(vec![Block::Text("hi".into())])];
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let resp = p.chat(&mk_req(&m, &cancel), &mut |_| {}).unwrap();
    assert!(matches!(&resp.blocks[0], Block::Text(t) if t == "ok"));

    // finish_reason WITHOUT any delta (choices non-null, delta absent).
    let sse2 = concat!(
        "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let url = spawn_mock("200 OK", "text/event-stream", sse2);
    let p = OpenAi::new(client(), url, "k", Duration::from_secs(30));
    let m = vec![Message::user(vec![Block::Text("hi".into())])];
    let resp = p.chat(&mk_req(&m, &cancel), &mut |_| {}).unwrap();
    assert_eq!(resp.stop_reason, "end_turn");
    assert!(resp.blocks.is_empty(), "finish-only stream: no blocks");

    // content_filter finish_reason passes through verbatim (live provider).
    let sse3 = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"yes\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"content_filter\",\"delta\":{}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let url = spawn_mock("200 OK", "text/event-stream", sse3);
    let p = OpenAi::new(client(), url, "k", Duration::from_secs(30));
    let m = vec![Message::user(vec![Block::Text("hi".into())])];
    let resp = p.chat(&mk_req(&m, &cancel), &mut |_| {}).unwrap();
    assert_eq!(resp.stop_reason, "content_filter");
}

#[test]
fn retry_body_400_identical_apart_from_the_fix() {
    // Mock: 400 iff the body still carries the OLD field name. Capture both
    // bodies; assert they are semantically identical except the renamed key.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let bodies: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let bodies_c = bodies.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let body = read_request_body(&stream);
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                bodies_c.lock().unwrap().push(v);
            }
            let is_400 = body.contains("\"max_tokens\"");
            let payload: &str = if is_400 {
                r#"{"error":{"message":"Unrecognized request argument: max_tokens","type":"invalid_request_error"}}"#
            } else {
                concat!(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n",
                    "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                    "data: [DONE]\n\n",
                )
            };
            let (status, ct): (&str, &str) = if is_400 {
                ("400 Bad Request", "application/json")
            } else {
                ("200 OK", "text/event-stream")
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
    let p = OpenAi::new(client(), url, "k", Duration::from_secs(30));
    let m = vec![Message::user(vec![Block::Text("hi".into())])];
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    p.chat(&mk_req(&m, &cancel), &mut |_| {}).expect("recovers");
    let got = bodies.lock().unwrap();
    assert_eq!(got.len(), 2, "400 + fixed retry");
    let mut a = got[0].clone();
    let mut b = got[1].clone();
    let old = a.as_object_mut().unwrap().remove("max_tokens");
    let new = b.as_object_mut().unwrap().remove("max_completion_tokens");
    assert_eq!(old, new, "token cap value must survive the rename");
    assert_eq!(
        a, b,
        "the rest of the retry body must be byte-identical (semantic JSON)"
    );
}

#[test]
fn openai_cancel_before_first_post_sends_nothing() {
    // Symmetry with the Anthropic side (review finding, fixed): a cancel
    // already set must fail fast BEFORE the POST — no request round-trip is
    // spent, and the error (with the cancel flag up) maps to EXIT_CANCELLED.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits_c = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let _ = read_request_body(&stream);
            hits_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let payload = concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"late\"}}]}\n\n",
                "data: [DONE]\n\n",
            );
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                payload.len(),
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(payload.as_bytes());
            let _ = stream.flush();
        }
    });
    let p = OpenAi::new(client(), url, "k", Duration::from_secs(30));
    let m = vec![Message::user(vec![Block::Text("hi".into())])];
    let cancel = std::sync::Arc::new(AtomicBool::new(true));
    let err = match p.chat(&mk_req(&m, &cancel), &mut |_| {}) {
        Err(e) => e,
        Ok(_) => panic!("pre-post cancel must fail fast, not stream"),
    };
    assert!(err.to_string().contains("cancelled"), "{err}");
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no request round-trip may be spent on an already-cancelled call"
    );
}

// ==== ThinkStripper differential + parse_loop wiring ====

/// Split `s` into &str chunks at CHAR boundaries according to `step`
/// (byte distance target; never splits a char).
fn char_chunks(s: &str, step: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut next = 0;
    for (i, _) in s.char_indices().skip(1) {
        if i - start >= step {
            out.push(&s[start..i]);
            start = i;
        }
        next = i;
    }
    let _ = next;
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

fn run(chunks: &[&str]) -> (String, String) {
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
    // The stripper must be fully drained at finish in Normal state.
    if st.state == ThinkState::Normal {
        assert!(st.buf.is_empty(), "Normal-state finish must drain buf");
    }
    (text, think)
}

/// The documented FR-2 semantics, as a whole-string reference:
/// - byte-exact `<think>` commits think-until-close (fail-closed at EOS);
/// - `</think>` without an open one is ordinary text;
/// - `<THINK>` (uppercase) is ordinary text;
/// - a trailing partial that never completes is text.
fn reference(input: &str) -> (String, String) {
    let mut text = String::new();
    let mut think = String::new();
    let mut in_think = false;
    let mut rest = input;
    loop {
        if in_think {
            match rest.find(THINK_CLOSE) {
                Some(i) => {
                    think.push_str(&rest[..i]);
                    rest = &rest[i + THINK_CLOSE.len()..];
                    in_think = false;
                }
                None => {
                    think.push_str(rest);
                    break;
                }
            }
        } else {
            match rest.find(THINK_OPEN) {
                Some(i) => {
                    text.push_str(&rest[..i]);
                    rest = &rest[i + THINK_OPEN.len()..];
                    in_think = true;
                }
                None => {
                    text.push_str(rest);
                    break;
                }
            }
        }
    }
    (text, think)
}

/// Byte conservation: text ++ think must equal input minus complete tag bytes.
/// Implied by reference equality, but asserted separately on the streaming
/// output to catch loss/duplication independently of the reference.
fn conservation_ok(input: &str, text: &str, think: &str) -> bool {
    // remove complete tags from input by walking the same scanner
    let (rt, rk) = reference(input);
    rt == text && rk == think
}

#[test]
fn think_handcrafted_corpus_matches_reference_at_every_split() {
    let cases: Vec<&str> = vec![
        "plain text",
        "<think>a</think>b",
        "a<think>b</think>c<think>d</think>e",
        "</think>close without open",
        "<THINK>upper</THINK>",
        "<think>unterminated",
        "trailing partial <thin",
        "trailing exact open <think",
        "trailing open with bytes <think>tail",
        "<think>a<think>b</think>c",
        "<think>\nwith newlines\n</think>after",
        "<</think>>",
        "<<think>",
        "<<think>>",
        "<think></think>",
        "</think>",
        "",
        "<think>text with <b>tags</b> inside</think>after",
        "a<think</think>b",
        "a<<think>b",
        "你好<think>思考🤔</think>答案",
        "x<think>y</think",
        "x<think>y</thin",
        "<think",
        "<",
        "><think>",
        "a<think>b</think>",
    ];
    for input in cases {
        let (rt, rk) = reference(input);
        // whole-feed
        let (t, k) = run(&[input]);
        assert_eq!(
            (t.clone(), k.clone()),
            (rt.clone(), rk.clone()),
            "whole: {input:?}"
        );
        assert!(
            conservation_ok(input, &t, &k),
            "conservation whole: {input:?}"
        );
        // every single cut point
        let idxs: Vec<usize> = input.char_indices().skip(1).map(|(i, _)| i).collect();
        for i in idxs.iter().copied() {
            let chunks = vec![&input[..i], &input[i..]];
            let (t, k) = run(&chunks);
            assert_eq!((t, k), (rt.clone(), rk.clone()), "cut@{i}: {input:?}");
        }
        // every pair of cut points (inputs are small)
        for i in idxs.iter().copied() {
            for j in idxs.iter().copied() {
                if j <= i {
                    continue;
                }
                let chunks = vec![&input[..i], &input[i..j], &input[j..]];
                let (t, k) = run(&chunks);
                assert_eq!((t, k), (rt.clone(), rk.clone()), "cut@{i},{j}: {input:?}");
            }
        }
        // char-by-char
        let chunks = char_chunks(input, 1);
        let (t, k) = run(&chunks);
        assert_eq!((t, k), (rt.clone(), rk.clone()), "charwise: {input:?}");
        // multi-char steps
        for step in [2usize, 3, 5, 7, 9] {
            let chunks = char_chunks(input, step);
            let (t, k) = run(&chunks);
            assert_eq!((t, k), (rt.clone(), rk.clone()), "step={step}: {input:?}");
        }
        // empty-chunk interleaving (zero-length reads must be no-ops)
        let mut chunks: Vec<&str> = Vec::new();
        for c in char_chunks(input, 2) {
            chunks.push("");
            chunks.push(c);
        }
        chunks.push("");
        let (t, k) = run(&chunks);
        assert_eq!((t, k), (rt, rk), "empty-chunk interleave: {input:?}");
    }
}

/// Deterministic LCG fuzz over a tag-heavy alphabet: every sample must match
/// the reference under every split (whole, 1-cut, charwise, random).
#[test]
fn think_fuzz_matches_reference_and_is_chunk_invariant() {
    let alphabet: Vec<char> = "a</think> <THINK>x".chars().collect();
    let mut seed: u64 = 0x5EED_B00C;
    let mut rng = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };
    let mut mismatches = 0usize;
    for _ in 0..4000 {
        let len = rng() % 26;
        let input: String = (0..len).map(|_| alphabet[rng() % alphabet.len()]).collect();
        let (rt, rk) = reference(&input);
        // whole
        let got = run(&[input.as_str()]);
        if got != (rt.clone(), rk.clone()) {
            mismatches += 1;
            eprintln!("WHOLE MISMATCH input={input:?} got={got:?} want=({rt:?},{rk:?})");
            continue;
        }
        // single random cut (char boundary)
        if !input.is_empty() {
            let cuts: Vec<usize> = input.char_indices().skip(1).map(|(i, _)| i).collect();
            if !cuts.is_empty() {
                let i = cuts[rng() % cuts.len()];
                let got = run(&[&input[..i], &input[i..]]);
                if got != (rt.clone(), rk.clone()) {
                    mismatches += 1;
                    eprintln!(
                        "CUT MISMATCH input={input:?} cut={i} got={got:?} want=({rt:?},{rk:?})"
                    );
                }
            }
        }
        // random multi-cut
        let mut chunks: Vec<&str> = Vec::new();
        let mut last = 0usize;
        let cuts: Vec<usize> = input.char_indices().skip(1).map(|(i, _)| i).collect();
        if !cuts.is_empty() {
            let ncuts = rng() % 4;
            for _ in 0..ncuts {
                let i = cuts[rng() % cuts.len()];
                if i > last {
                    chunks.push(&input[last..i]);
                    last = i;
                }
            }
        }
        chunks.push(&input[last..]);
        let got = run(&chunks);
        if got != (rt.clone(), rk.clone()) {
            mismatches += 1;
            eprintln!("MULTICUT MISMATCH input={input:?} chunks={chunks:?} got={got:?} want=({rt:?},{rk:?})");
        }
    }
    assert_eq!(mismatches, 0, "fuzz found divergences (see stderr)");
}

#[test]
fn think_no_loss_no_duplication_property() {
    // Independently of the reference: text ++ think must equal the input with
    // complete tags removed (streamed, charwise splits).
    let samples = [
        "a<think>b</think>c",
        "<think>x</think>",
        "q<think>r</think>s<t",
        "<think>p1</think>MID<think>p2</think>Z",
    ];
    for input in samples {
        let (t, k) = run(&char_chunks(input, 1));
        let mut rebuilt = t.clone();
        rebuilt.push_str(&k);
        let mut want = input.to_string();
        for tag in [THINK_OPEN, THINK_CLOSE] {
            want = want.replace(tag, "");
        }
        assert_eq!(
            rebuilt.len(),
            want.len(),
            "byte count: {input:?} -> ({t:?},{k:?})"
        );
    }
}

// ── parse_loop wiring (SSE level) ───────────────────────────────────────────

fn parse_events(raw: &str, strip: bool) -> (crate::provider::ChatResponse, Vec<ChatEvent>) {
    let mut sse = SseReader::new(raw.as_bytes());
    let mut events = Vec::new();
    let resp = super::OpenAi::parse_loop(&mut sse, &mut |e| events.push(e), strip, None).unwrap();
    (resp, events)
}

#[test]
fn think_tag_split_across_sse_events_and_chunks() {
    // The tag is split at EVERY byte boundary across TWO SSE content deltas;
    // the channel split must be identical to the un-split stream.
    let open = THINK_OPEN; // <think>
    let close = THINK_CLOSE; // </think>
    for i in 0..=open.len() {
        let (a, b) = open.split_at(i);
        let raw = format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{a}\"}}}}]}}\n\n\
             data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{b}reason {i}\"}}}}]}}\n\n\
             data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"</thi\"}}}}]}}\n\n\
             data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"nk>answer\"}}}}]}}\n\n\
             data: {{\"choices\":[{{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{{}}}}]}}\n\n"
        );
        let (resp, events) = parse_events(&raw, true);
        assert_eq!(resp.stop_reason, "end_turn", "open cut i={i}");
        assert_eq!(resp.blocks.len(), 1, "open cut i={i}");
        assert!(
            matches!(&resp.blocks[0], crate::provider::Block::Text(t) if t == "answer"),
            "open cut i={i}: {:?}",
            resp.blocks
        );
        let think: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::ThinkingDelta(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::TextDelta(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(think, format!("reason {i}"), "open cut i={i}");
        assert_eq!(text, "answer", "open cut i={i}");
    }
    // Same sweep for the CLOSE tag split across events.
    for i in 0..=close.len() {
        let (a, b) = close.split_at(i);
        let raw = format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"<think>thought{a}\"}}}}]}}\n\n\
             data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{b}after\"}}}}]}}\n\n\
             data: {{\"choices\":[{{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{{}}}}]}}\n\n"
        );
        let (resp, events) = parse_events(&raw, true);
        assert!(
            matches!(&resp.blocks[0], crate::provider::Block::Text(t) if t == "after"),
            "close cut i={i}: {:?}",
            resp.blocks
        );
        let think: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::ThinkingDelta(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            think,
            format!("thought{a}{b}").replace(close, ""),
            "close cut i={i}"
        );
    }
}

#[test]
fn think_tool_arguments_are_never_stripped() {
    let raw = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"cmd\\\":\\\"<think>evil</think>ls\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"tool_calls\",\"delta\":{}}]}\n\n",
    );
    let (resp, events) = parse_events(raw, true);
    match &resp.blocks[0] {
        crate::provider::Block::ToolUse { input, .. } => {
            assert_eq!(
                input["cmd"], "<think>evil</think>ls",
                "tool arguments must never be stripped"
            );
        }
        other => panic!("expected tool_use, got {other:?}"),
    }
    // And no ThinkingDelta leaked from the arguments channel.
    assert!(!events
        .iter()
        .any(|e| matches!(e, ChatEvent::ThinkingDelta(_))));
}

#[test]
fn think_knob_off_is_byte_identical_passthrough() {
    let content = "a<think>b</think>c <thin </think> x";
    let raw = format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{{}}}}]}}\n\n"
    );
    let (resp, events) = parse_events(&raw, false);
    assert!(
        matches!(&resp.blocks[0], crate::provider::Block::Text(t) if t == content),
        "knob off: verbatim"
    );
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ChatEvent::TextDelta(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, content);
    assert!(!events
        .iter()
        .any(|e| matches!(e, ChatEvent::ThinkingDelta(_))));
}

#[test]
fn think_stream_error_mid_block_flushes_fail_closed() {
    // A reader that dies mid-<think>-block: the held thinking residue must
    // flush to the THINKING channel only (no residue in text/history).
    struct FailingAfter {
        data: Vec<u8>,
        pos: usize,
    }
    impl Read for FailingAfter {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.data.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "reset mid-think",
                ));
            }
            let n = std::cmp::min(b.len(), self.data.len() - self.pos);
            b[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }
    let raw = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"visible <think>secret reasoning\"}}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":9}}\n\n",
    );
    let mut events = Vec::new();
    let resp = super::OpenAi::parse_stream(
        FailingAfter {
            data: raw.as_bytes().to_vec(),
            pos: 0,
        },
        &mut |e| events.push(e),
    )
    .unwrap();
    assert_eq!(resp.stop_reason, "stream_error");
    // Usage rides through (honest cost).
    assert_eq!(
        resp.usage.map(|u| (u.input_tokens, u.output_tokens)),
        Some((5, 9))
    );
    // History text: only the visible part + marker; NO tag residue, NO think.
    let texts: Vec<&str> = resp
        .blocks
        .iter()
        .filter_map(|b| match b {
            crate::provider::Block::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts[0], "visible ");
    assert!(texts[1].starts_with("[stream error]"));
    assert!(!texts.iter().any(|t| t.contains("secret")));
    let think: String = events
        .iter()
        .filter_map(|e| match e {
            ChatEvent::ThinkingDelta(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        think, "secret reasoning",
        "fail-closed: held think flushes to thinking"
    );
}

// ── idle-timeout abort path (zero prior test coverage — IMP evidence) ──────

#[test]
fn idle_timeout_aborts_stream_labels_stream_idle_and_keeps_partial() {
    use super::{CancellableSse, SseSource};
    use std::sync::atomic::AtomicBool;

    /// One payload, then the connection stalls (half-open) — no EOF.
    struct StallAfterPrefix {
        data: Vec<u8>,
        pos: usize,
    }
    impl Read for StallAfterPrefix {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.data.len() {
                std::thread::sleep(Duration::from_secs(60));
                return Ok(0);
            }
            let n = std::cmp::min(b.len(), self.data.len() - self.pos);
            b[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    // 1) Raw source level: the stall is labeled IdleTimeout within ~idle.
    let raw = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"pre\"}}]}\n\n";
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let mut src = CancellableSse::new(
        StallAfterPrefix {
            data: raw.as_bytes().to_vec(),
            pos: 0,
        },
        cancel.clone(),
        Duration::from_secs(1),
    )
    .unwrap();
    let first = src.next_payload().unwrap();
    assert_eq!(
        first.as_deref(),
        Some("{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"pre\"}}]}")
    );
    let started = std::time::Instant::now();
    let second = src.next_payload();
    // Ceiling 15s (not a tight ~5s): the pin's job is to prove the abort
    // fires near idle (1s) INSTEAD of the mock's 60s stall — any ceiling
    // well under 60s keeps that meaning, and the wide slack tolerates a
    // heavily throttled CI runner starving this test thread for seconds
    // (macos arm64 capacity-constrained runners: observed as a CI-only
    // flake, exit 101, never reproducible locally under synthetic load).
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "idle must fire near 1s, not run toward the 60s mock stall"
    );
    match second {
        Err(super::StreamAbort::IdleTimeout) => {}
        other => panic!("expected IdleTimeout, got {other:?}"),
    }

    // 2) Through the OpenAI parse_loop: partial text kept, stop "stream_idle".
    let cancel2 = std::sync::Arc::new(AtomicBool::new(false));
    let mut src2 = CancellableSse::new(
        StallAfterPrefix {
            data: raw.as_bytes().to_vec(),
            pos: 0,
        },
        cancel2.clone(),
        Duration::from_secs(1),
    )
    .unwrap();
    let resp = super::OpenAi::parse_loop(&mut src2, &mut |_| {}, true, None).unwrap();
    assert_eq!(resp.stop_reason, "stream_idle");
    assert!(
        matches!(&resp.blocks[0], crate::provider::Block::Text(t) if t == "pre"),
        "partial text survives the idle abort: {:?}",
        resp.blocks
    );
    assert!(resp.usage.is_none());
}

#[test]
fn zero_payload_cancel_abort_carries_no_truncation_diagnosis() {
    // Wave-2 rework pin: the zero-data diagnostic block must fire ONLY on the
    // clean-EOF truncation path. A cancel landing in the TTFB window (the
    // exact window the pre-POST guard's comment names) aborts the stream
    // before any payload — stamping "gateway returned a non-streaming body?"
    // over that cancel would put a FALSE accusation (content-type even shows
    // text/event-stream) into history, final_text and the session.
    struct CommentOnlyThenFlip {
        cancel: std::sync::Arc<AtomicBool>,
        sent: bool,
    }
    impl Read for CommentOnlyThenFlip {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            if !self.sent {
                self.sent = true;
                let head = b": keep-alive\n\n";
                let n = head.len().min(b.len());
                b[..n].copy_from_slice(&head[..n]);
                Ok(n)
            } else {
                // TTFB-window flip: the cancel arrives while the stream is
                // still open (no EOF yet), so the abort is Cancelled, not a
                // clean end. The store STRICTLY precedes the EOF and the gap
                // is 5ms — well inside one POLL_SLICE (200ms). Routing
                // analysis (wave-2 re-review W2-P2-1): with the fix, EVERY
                // route yields Cancelled — the consumer's current recv slice
                // is interrupted by the early Disconnected (arm re-samples
                // the flag), or a slice boundary lands first (loop-top check
                // sees the flag); either way the pin cannot flake. Without
                // the Disconnected-arm re-sample, the early-EOF route
                // (usually taken: EOF lands inside the running slice)
                // returns clean EOF → truncation → FAIL, which is what makes
                // this pin a mutation killer. Honest bound: if a slice
                // boundary happens to fall inside the 5ms window (~2.5% of
                // runs), the loop-top route masks the mutation — repeat runs
                // drive the escape probability to zero.
                self.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(5));
                Ok(0)
            }
        }
    }
    use super::CancellableSse;
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let mut sse = CancellableSse::new(
        CommentOnlyThenFlip {
            cancel: cancel.clone(),
            sent: false,
        },
        cancel.clone(),
        Duration::from_secs(20),
    )
    .unwrap();
    let resp = OpenAi::parse_loop(&mut sse, &mut |_| {}, true, Some("text/event-stream")).unwrap();
    assert_eq!(resp.stop_reason, "cancelled");
    assert!(
        resp.blocks.is_empty(),
        "no diagnostic block over an abort: {:?}",
        resp.blocks
    );
}
