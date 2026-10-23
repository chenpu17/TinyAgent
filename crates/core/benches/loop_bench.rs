//! Zero-dependency performance benchmarks for the tiny-agent kernel.
//!
//! Deliberately hand-rolled on `std::time::Instant` — the project ships a
//! single, minimal binary and even its mock servers are dependency-free, so a
//! benchmark harness crate (criterion & co.) would be the only dev-dependency
//! in the tree for numbers a few dozen lines can produce. `harness = false`
//! means cargo runs `main` directly; no libtest is linked.
//!
//! What is measured (public API only — the kernel's private helpers stay
//! private for benchmarks too):
//!
//! * `loop_turn_*` — one full `Agent::run` turn driven by the scripted
//!   `provider::mock::MockProvider` (network-free floor for loop overhead: the
//!   mock's own request capture is included, so these are upper bounds). The
//!   agent is constructed outside the timer, so the number is the loop's own
//!   cost; `agent_construct_long` then times `Agent::new` separately (the
//!   once-per-session cost hosts pay on resume / sub-agent spawn). Only
//!   available with `--features mock`; a feature-off build prints a skip note
//!   and keeps going (the same build-adaptive discipline as the Node binding
//!   tests).
//! * `context_breakdown` — the host UI's pull query, on a short and a
//!   200-message history (a per-turn cost hosts pay on every refresh).
//! * `confined_to` — the file-tool sandbox walk, on the success path (plain
//!   and through an in-tree symlink) and on an escape attempt.
//!
//! Output: a human table plus one stable machine-readable line per case:
//! `BENCH name=<case> median_ns=<n> p95_ns=<n> samples=<n> batch=<n>`.
//! Median/p95, not the mean — one scheduler blip must not move the headline.
//!
//! `cargo bench`'s bench profile inherits `[profile.release]` (opt-level="z" +
//! LTO, sized for the release binary); the workspace root overrides it with
//! `[profile.bench] opt-level = 3, lto = false` so the numbers describe the
//! code, not the binary-size trade-off. Do not remove that override.
//!
//! Usage:
//!   cargo bench -p tiny-agent-core --features mock --bench loop_bench
//!   cargo bench -p tiny-agent-core --features mock --bench loop_bench -- --quick

use std::time::Instant;

/// Per-sample target: short enough that a ~µs operation gets a batch large
/// enough to dwarf `Instant::now` overhead, long enough not to pay the
/// timer syscall on every single call.
const SAMPLE_TARGET_NS: u128 = 20_000;

fn human_ns(ns: u128) -> String {
    if ns >= 1_000_000 {
        format!("{:.2}ms", ns as f64 / 1e6)
    } else if ns >= 1_000 {
        format!("{:.2}µs", ns as f64 / 1e3)
    } else {
        format!("{ns}ns")
    }
}

/// Run one case: calibrate, warm up, sample, print the table row and the
/// machine-readable line. `batch` iterations are timed together and divided,
/// so per-call timer overhead stays negligible even for sub-microsecond work.
fn measure(name: &str, quick: bool, mut f: impl FnMut()) -> u128 {
    let budget_ns: u128 = if quick { 150_000_000 } else { 1_000_000_000 };

    // Estimate with the MIN of three single shots — the least noisy estimator
    // under scheduler interference; the batch size is not correctness-critical.
    let mut est = u128::MAX;
    for _ in 0..3 {
        let t = Instant::now();
        f();
        est = est.min(t.elapsed().as_nanos().max(1));
    }
    let batch = (SAMPLE_TARGET_NS / est).clamp(1, 1000) as usize;
    // Hard cap: the estimate is a LOW bound (min of single shots), so the
    // sample count is what keeps a slow case from turning `--quick` into a
    // multi-minute run even when the estimate ran optimistic.
    let samples = (budget_ns / (est * batch as u128)).clamp(20, 2000) as usize;

    // Warm up the same path (allocator arenas, page cache, branch predictors).
    // Deliberately not counted: the first calls of anything are slower for
    // reasons unrelated to the code under test.
    for _ in 0..(samples / 5).max(1) {
        for _ in 0..batch {
            f();
        }
    }

    let mut times = Vec::with_capacity(samples);
    for _ in 0..samples {
        let t = Instant::now();
        for _ in 0..batch {
            f();
        }
        times.push(t.elapsed().as_nanos() / batch as u128);
    }
    times.sort_unstable();
    let median_ns = times[times.len() / 2];
    // Nearest-rank p95 over the sorted samples.
    let p95_ns = times[((times.len() as f64 - 1.0) * 0.95).round() as usize];

    println!(
        "{:<30} {:>10} {:>10} {:>9}",
        name,
        human_ns(median_ns),
        human_ns(p95_ns),
        times.len()
    );
    println!(
        "BENCH name={name} median_ns={median_ns} p95_ns={p95_ns} samples={} batch={batch}",
        times.len()
    );
    median_ns
}

/// Like [`measure`], but with an untimed per-iteration setup step: `setup`
/// produces fresh state, `op` mutates it, and only `op` is timed. Used where
/// the per-call state is expensive to build for reasons unrelated to the code
/// under test (e.g. `Agent::new` over a large history), so the sample measures
/// the operation and not the constructor.
///
/// Gated on `mock`: only the agent cases need it, and an unused helper would
/// warn as dead code in the feature-off build.
#[cfg(feature = "mock")]
fn measure_with<R>(
    name: &str,
    quick: bool,
    mut setup: impl FnMut() -> R,
    mut op: impl FnMut(&mut R),
) -> u128 {
    let budget_ns: u128 = if quick { 150_000_000 } else { 1_000_000_000 };
    let est = |setup: &mut dyn FnMut() -> R, op: &mut dyn FnMut(&mut R)| -> u128 {
        let mut best = u128::MAX;
        for _ in 0..3 {
            let mut state = setup();
            let t = Instant::now();
            op(&mut state);
            best = best.min(t.elapsed().as_nanos().max(1));
        }
        best
    };
    let e = est(&mut setup, &mut op);
    let samples = (budget_ns / e).clamp(20, 2000) as usize;
    for _ in 0..(samples / 5).max(1) {
        let mut state = setup();
        op(&mut state);
    }

    let mut times = Vec::with_capacity(samples);
    for _ in 0..samples {
        let mut state = setup();
        let t = Instant::now();
        op(&mut state);
        times.push(t.elapsed().as_nanos());
    }
    times.sort_unstable();
    let median_ns = times[times.len() / 2];
    let p95_ns = times[((times.len() as f64 - 1.0) * 0.95).round() as usize];
    println!(
        "{:<30} {:>10} {:>10} {:>9}",
        name,
        human_ns(median_ns),
        human_ns(p95_ns),
        times.len()
    );
    println!(
        "BENCH name={name} median_ns={median_ns} p95_ns={p95_ns} samples={} batch=1",
        times.len()
    );
    median_ns
}

fn header() {
    println!(
        "{:<30} {:>10} {:>10} {:>9}",
        "case", "median", "p95", "samples"
    );
    println!("{}", "-".repeat(63));
}

fn main() {
    let mut quick = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--quick" => quick = true,
            "-h" | "--help" => {
                println!(
                    "usage: loop_bench [--quick]\n\
                     \n\
                     --quick  sample ~150ms per case instead of ~1s\n\
                     \n\
                     The agent-loop cases need the `mock` cargo feature:\n\
                     cargo bench -p tiny-agent-core --features mock --bench loop_bench"
                );
                return;
            }
            // cargo appends `--bench` to a harness-less target by itself;
            // accept it as a no-op so `cargo bench ... -- --quick` works.
            "--bench" => {}
            other => {
                eprintln!("loop_bench: unknown argument `{other}` (try --help)");
                std::process::exit(2);
            }
        }
    }

    header();
    #[cfg(feature = "mock")]
    agent_bench::run(quick);
    #[cfg(not(feature = "mock"))]
    {
        println!("skipped: agent-loop / context_breakdown cases need --features mock");
        println!("         cargo bench -p tiny-agent-core --features mock --bench loop_bench");
        for name in [
            "loop_turn_empty",
            "loop_turn_long_history",
            "agent_construct_long",
            "context_breakdown_short",
            "context_breakdown_long",
        ] {
            println!("BENCH name={name} skipped=needs-mock-feature");
        }
    }
    tool_bench::run(quick);
}

/// The `--features mock` half. Everything mock-dependent lives in this module
/// so the feature-off bench has no dangling imports to warn about.
#[cfg(feature = "mock")]
mod agent_bench {
    use super::{measure, measure_with};
    use std::hint::black_box;
    use tiny_agent::provider::mock::{MockProvider, MockTurn};
    use tiny_agent::{Agent, Block, ChatResponse, Config, Message, ToolCtx};

    /// A bench-local config. Same shape as the kernel's test helper
    /// (`agent_tests::cfg_with`) but written here: benchmarks may only touch
    /// public API, and must not reach into the test module for helpers.
    /// Compression stays far above the bench history sizes so a measured turn
    /// never turns into a billed summary call (MockProvider would serve it
    /// an `end_turn` text reply and the numbers would mean something else).
    fn bench_config() -> Config {
        serde_json::from_str(
            r#"{
                "provider": "openai",
                "api_key_env": "BENCH_API_KEY",
                "model": "bench-model",
                "inject_environment": false,
                "limits": {
                    "max_turns": 4,
                    "timeout_secs": 60,
                    "compress_threshold_chars": 100000000
                }
            }"#,
        )
        .expect("bench config must parse")
    }

    fn bench_ctx() -> ToolCtx {
        // All fields are public by contract (the bindings and sub-agents build
        // this by hand). No tools are registered, so the ctx is inert here.
        ToolCtx {
            workdir: std::env::current_dir().expect("cwd"),
            max_output_chars: 30_000,
            restrict_to_workdir: false,
            cancel: None,
            shell_timeout_secs: 120,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
            tool_absent_notes: Vec::new(),
        }
    }

    /// One scripted turn: a plain text reply that ends the run.
    fn text_reply() -> MockTurn {
        MockTurn::Ok(ChatResponse {
            blocks: vec![Block::Text("bench reply".into())],
            stop_reason: "end_turn".into(),
            usage: None,
        })
    }

    fn agent_over(initial: Vec<Message>) -> Agent {
        let (provider, _state) = MockProvider::new(vec![text_reply()]);
        Agent::new(
            &bench_config(),
            Box::new(provider),
            Vec::new(),
            initial,
            bench_ctx(),
        )
    }

    /// One user turn — the shortest legal history to run from.
    fn short_history() -> Vec<Message> {
        vec![Message::user(vec![Block::Text(
            "bench task: reply with one short sentence".into(),
        )])]
    }

    /// 200 messages (−100 real back-and-forth pairs plus the closing task),
    /// each body ~300 chars — representative of a grown conversation and big
    /// enough that an O(n²) per-turn scan over the history would stand out
    /// against the empty-history case.
    fn long_history() -> Vec<Message> {
        let body = "The quick brown fox jumps over the lazy dog. ".repeat(7);
        let mut msgs = Vec::with_capacity(200);
        for i in 0..99 {
            msgs.push(Message::user(vec![Block::Text(format!(
                "question {i}: {body}"
            ))]));
            msgs.push(Message::assistant(vec![Block::Text(format!(
                "answer {i}: {body}"
            ))]));
        }
        msgs.push(Message::user(vec![Block::Text(
            "bench task: reply with one short sentence".into(),
        )]));
        debug_assert_eq!(msgs.len(), 200);
        msgs
    }

    /// Run exactly one turn and assert it really was one clean turn — a bench
    /// that silently measured a cancellation or an error would look great and
    /// mean nothing.
    fn one_turn(agent: &mut Agent) {
        let outcome = agent
            .run(&mut |_| {})
            .expect("scripted single-turn run must succeed");
        assert_eq!(outcome.turns, 1, "expected exactly one turn");
        assert_eq!(outcome.stop_reason, "end_turn");
        black_box(&outcome);
    }

    pub fn run(quick: bool) {
        let short = short_history();
        let long = long_history();

        // Case 1 — per-turn cost on a one-message history. `Agent::new` runs
        // per iteration (fresh state, no history growth between samples) but
        // outside the timer: this is the loop+provider-mock overhead, not the
        // constructor.
        let empty_median = measure_with(
            "loop_turn_empty",
            quick,
            || agent_over(short.clone()),
            one_turn,
        );

        // Case 2 — the same turn with 200 messages behind it. Pre-built per
        // sample for the same reason as above, so the ratio against case 1 is
        // the loop's own history-dependent cost (not construction).
        let long_median = measure_with(
            "loop_turn_long_history",
            quick,
            || agent_over(long.clone()),
            one_turn,
        );

        // Case 2b — `Agent::new` over the 200-message history, timed on its
        // own. The host pays this once per session (resume, sub-agent spawn),
        // so it is a different axis from the per-turn numbers; reported
        // separately because a superlinear cost here must not be mistaken for
        // loop overhead. Ids are minted at ingest (each message is examined
        // for an existing id), so this is where a history-wide scan per
        // message would show up.
        let construct_median = measure("agent_construct_long", quick, || {
            black_box(agent_over(long.clone()));
        });

        // Case 3 — context_breakdown: pulled by the host on every UI refresh.
        // Built once per shape (read-only query, no history growth).
        let short_agent = agent_over(short.clone());
        let long_agent = agent_over(long.clone());
        let short_bd = short_agent.context_breakdown();
        let long_bd = long_agent.context_breakdown();
        let bd_short_median = measure("context_breakdown_short", quick, || {
            black_box(short_agent.context_breakdown());
        });
        let bd_long_median = measure("context_breakdown_long", quick, || {
            black_box(long_agent.context_breakdown());
        });

        // Ratios over medians: linear work over a 200x-longer history should
        // land in the single digits; anything in the hundreds is quadratic.
        println!(
            "info: long/short median ratio: loop_turn {:.2}x, context_breakdown {:.2}x",
            ratio(long_median, empty_median),
            ratio(bd_long_median, bd_short_median),
        );
        println!(
            "info: agent_construct_long={} (one-off host cost, excluded from loop ratios)",
            super::human_ns(construct_median)
        );
        println!(
            "info: context total_tokens: short={} long={} (setup sanity)",
            short_bd.total_tokens, long_bd.total_tokens
        );
    }

    fn ratio(a: u128, b: u128) -> f64 {
        a as f64 / b.max(1) as f64
    }
}

/// Tool-layer cases: pure filesystem work, available on every build.
mod tool_bench {
    use super::measure;
    use std::hint::black_box;
    use std::path::{Path, PathBuf};
    use tiny_agent::tools::confined_to;

    /// Self-cleaning temp tree — a panic in the bench must not leave a bench
    /// directory behind in the system temp dir.
    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "tiny-agent-bench-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(root.join("sub/deep")).expect("create bench tree");
            std::fs::write(root.join("sub/deep/file.txt"), b"bench").expect("write bench file");
            // An IN-TREE symlink: resolving it must still land inside root.
            #[cfg(unix)]
            std::os::unix::fs::symlink(root.join("sub"), root.join("link"))
                .expect("create in-tree symlink");
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    pub fn run(quick: bool) {
        let tree = TempTree::new();
        let root = tree.path();

        // (a) Success path: an existing file below the root. Every file-tool
        // call walks this; it canonicalizes root + the deepest existing
        // ancestor and re-checks containment per landing.
        let ok = root.join("sub/deep/file.txt");
        assert!(confined_to(root, &ok, "workdir").is_ok(), "ok path");
        measure("confined_to_ok", quick, || {
            black_box(confined_to(root, &ok, "workdir").is_ok());
        });

        // (a') Same, with an in-tree symlink as an intermediate component —
        // the symlink walk is the expensive half of the success path.
        #[cfg(unix)]
        {
            let linked = root.join("link/deep/file.txt");
            assert!(
                confined_to(root, &linked, "workdir").is_ok(),
                "symlink path"
            );
            measure("confined_to_ok_symlink", quick, || {
                black_box(confined_to(root, &linked, "workdir").is_ok());
            });
        }

        // (b) Rejection path: `..` escaping the workdir. The deepest existing
        // ancestor (the temp dir) is outside root, so this fails early — the
        // cheap half of the sandbox's cost.
        let escape = root.join("..").join("escape-target");
        assert!(
            confined_to(root, &escape, "workdir").is_err(),
            "escape path must be rejected"
        );
        measure("confined_to_escape", quick, || {
            black_box(confined_to(root, &escape, "workdir").is_err());
        });
    }
}
