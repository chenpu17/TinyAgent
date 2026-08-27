    use super::*;
    use crate::compress::*;
    use crate::hooks::{Hooks, ToolDecision};
    use crate::provider::mock::{self, MockProvider, MockTurn};
    use crate::provider::{ChatRequest, ChatResponse};
    use crate::tools::ToolOutput;
    use serde_json::json;

    fn user_text(s: &str) -> Message {
        Message::user(vec![Block::Text(s.into())])
    }

    fn tool_turn(id: &str) -> Vec<Message> {
        vec![
            Message::assistant(vec![Block::ToolUse {
                id: id.into(),
                name: "shell".into(),
                input: json!({"command": "ls"}),
            }]),
            Message::user(vec![Block::ToolResult {
                tool_use_id: id.into(),
                content: "ok".into(),
                image: None,
                is_error: false,
            }]),
        ]
    }

    #[test]
    fn safe_cut_skips_tool_pairs() {
        // [task][user a][tool pair][user b][tool pair at tail]
        let mut msgs = vec![user_text("task"), user_text("a")];
        msgs.extend(tool_turn("t1"));
        msgs.push(user_text("b"));
        msgs.extend(tool_turn("t2"));
        // indices: 0 task, 1 a, 2 asst-tu, 3 user-tr, 4 b, 5 asst-tu, 6 user-tr
        // keep_recent=2 → cut at 5 (assistant w/ tool_use); its tool_result
        // at index 6 stays in the tail — pair intact.
        assert_eq!(find_safe_cut(&msgs, 2), Some(5));
        // keep_recent=4 → candidate 3 (tool_result user) → slides past 4
        // (plain user) to 5 (assistant).
        assert_eq!(find_safe_cut(&msgs, 4), Some(5));
    }

    #[test]
    fn safe_cut_slides_past_tool_results() {
        // Pure tool loop: [task][asst tu][user tr] × 4
        let mut msgs = vec![user_text("task")];
        for i in 0..4 {
            msgs.extend(tool_turn(&format!("t{i}")));
        }
        // indices: 0 task, 1 asst, 2 user-tr, 3 asst, 4 user-tr, ...
        // keep_recent=2 → candidate cut at 7 = assistant (pair t3 intact).
        assert_eq!(find_safe_cut(&msgs, 2), Some(7));
        // keep_recent=6 → candidate cut at 3 = assistant.
        assert_eq!(find_safe_cut(&msgs, 6), Some(3));
    }

    #[test]
    fn append_user_turn_merges_into_trailing_user() {
        // History ends in a user message (e.g. the prior turn was cancelled
        // before any assistant text, so its assistant message was dropped).
        // The follow-up must merge, not append a second user message — two
        // adjacent user messages are a 400.
        let mut msgs = vec![user_text("task")];
        append_user_turn(&mut msgs, vec![Block::Text("again".into())]);
        assert_eq!(msgs.len(), 1, "no second user message");
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[0].content.len(), 2, "blocks merged into the one turn");
    }

    #[test]
    fn append_user_turn_pushes_new_after_assistant() {
        // Normal turn boundary: assistant spoke, now a user follow-up starts a
        // new message (no trailing user to merge into).
        let mut msgs = vec![
            user_text("task"),
            Message::assistant(vec![Block::Text("hi".into())]),
        ];
        append_user_turn(&mut msgs, vec![Block::Text("more".into())]);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2].role, Role::User);
    }

    #[test]
    fn safe_cut_too_short_history() {
        let msgs = vec![user_text("task"), user_text("hello"), user_text("more")];
        assert_eq!(find_safe_cut(&msgs, 8), None); // n <= keep_recent + 2
    }

    #[test]
    fn safe_cut_works_with_keep_recent_zero() {
        // keep_recent=0 means "the tail may be empty": cut==n is a legal cut —
        // fold_summary builds head + messages[n..], i.e. everything except the
        // retained head folds into the summary. Both shapes must compress:
        // assistant-final (any gateway that ended on an assistant message) and
        // user-final (the real loop's shape at every request point) — the old
        // clamp-to-n-1 fallback made the boundary scan miss on user-final and
        // silently DISABLED compression for keep_recent=0 configs.
        // [user, asst, user, asst] — n=4, guard (4 <= 0+2) passes.
        let msgs = vec![
            user_text("task"),
            Message::assistant(vec![Block::Text("r1".into())]),
            user_text("more"),
            Message::assistant(vec![Block::Text("r2".into())]),
        ];
        assert_eq!(find_safe_cut(&msgs, 0), Some(4));
        // user-final — the shape the real loop guarantees at request points.
        let msgs2 = vec![
            user_text("task"),
            Message::assistant(vec![Block::Text("r1".into())]),
            user_text("more"),
            user_text("again"),
        ];
        assert_eq!(find_safe_cut(&msgs2, 0), Some(4));
    }

    #[test]
    fn safe_cut_saturates_on_pathological_keep_recent() {
        // keep_recent=usize::MAX must disable compression, not wrap: a plain
        // `keep_recent + 2` panics in debug (and in release the guard passes
        // for large n, making `n - keep_recent` underflow into a giant cut).
        let msgs = vec![user_text("a"), user_text("b"), user_text("c")];
        assert_eq!(find_safe_cut(&msgs, usize::MAX), None);
    }

    #[test]
    fn fold_summary_empty_tail_is_legal() {
        // cut == messages.len() (keep_recent=0): the compressed history is
        // exactly one user message — the retained head carrying the summary.
        let msgs = vec![
            user_text("task"),
            Message::assistant(vec![Block::Text("r1".into())]),
            user_text("more"),
            user_text("again"),
        ];
        let out = fold_summary(&msgs, msgs.len(), "the summary");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, Role::User);
        assert_eq!(out[0].content.len(), 2); // task + summary
        assert!(matches!(&out[0].content[1], Block::Text(t) if t.contains("the summary")));
    }

    #[test]
    fn safe_cut_needs_middle_content() {
        // Candidate lands at index 1 (assistant) → nothing to summarize → None
        let msgs = vec![
            user_text("task"),
            Message::assistant(vec![Block::Text("r1".into())]),
            Message::assistant(vec![Block::Text("r2".into())]),
            Message::assistant(vec![Block::Text("r3".into())]),
            Message::assistant(vec![Block::Text("r4".into())]),
        ];
        assert_eq!(find_safe_cut(&msgs, 3), None);
    }

    #[test]
    fn fold_summary_replaces_old_summary_keeps_tail() {
        // Simulate a second compression: head already carries a summary
        // block; folding a new one must replace it, not stack.
        let mut msgs = vec![user_text("task"), user_text("old turn")];
        msgs = fold_summary(&msgs, 2, "summary-one");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content.len(), 2); // task + summary

        msgs.extend(tool_turn("t9"));
        let cut = 1; // drop everything between head and the tail
        let out = fold_summary(&msgs, cut, "summary-two");
        // head + 2 tail messages
        assert_eq!(out.len(), 3);
        // head: task + exactly one summary block, and it's the new one
        let summaries: Vec<_> = out[0]
            .content
            .iter()
            .filter(|b| matches!(b, Block::Text(t) if t.starts_with(SUMMARY_MARKER)))
            .collect();
        assert_eq!(summaries.len(), 1);
        assert!(matches!(summaries[0], Block::Text(t) if t.contains("summary-two")));
        assert!(matches!(&out[0].content[0], Block::Text(t) if t == "task"));
        // tail untouched
        assert_eq!(out[1].role, Role::Assistant);
        assert_eq!(out[2].role, Role::User);
    }

    #[test]
    fn fold_summary_uses_fresh_user_head_when_first_message_is_assistant() {
        // A resumed/edited session can start with an assistant message. The
        // summary must NOT be grafted onto that assistant head (it would break
        // user→assistant cadence and Anthropic rejects assistant-led exchanges
        // after the fold). Instead a fresh empty user head carries the summary.
        let msgs = vec![
            Message::assistant(vec![Block::Text("stray opening".into())]),
            user_text("real turn"),
            Message::assistant(vec![Block::Text("reply".into())]),
        ];
        let out = fold_summary(&msgs, 2, "the summary");
        // head + 1 tail message (messages[2..])
        assert_eq!(out.len(), 2);
        // Head is a USER message (fresh), not the assistant that was messages[0].
        assert_eq!(out[0].role, Role::User);
        // The fresh head carries ONLY the summary block (no stray opening text).
        assert_eq!(out[0].content.len(), 1);
        assert!(matches!(&out[0].content[0], Block::Text(t) if t.contains("the summary")));
        // Tail preserved as-is.
        assert_eq!(out[1].role, Role::Assistant);
    }

    #[test]
    fn total_chars_counts_blocks() {
        let msgs = vec![
            user_text("12345"),
            Message::assistant(vec![Block::ToolUse {
                id: "x".into(),
                name: "s".into(),
                input: json!({"a": 1}),
            }]),
        ];
        // "12345" (5) + "{\"a\":1}" (7)
        assert_eq!(total_chars(&msgs), 12);
    }

    #[test]
    fn total_chars_counts_tool_result_image() {
        // A load_image result carries its image inside ToolResult.image;
        // that payload must count toward the compress threshold just like
        // a top-level Block::Image, or image-heavy chats never compress.
        let msgs = vec![Message::user(vec![
            Block::Text("note".into()), // 4
            Block::ToolResult {
                tool_use_id: "x".into(),
                content: "see image".into(), // 9
                image: Some(Box::new(Block::Image {
                    media_type: "image/png".into(),
                    data: "A".repeat(1000), // 1000 / 100 = 10
                })),
                is_error: false,
            },
        ])];
        // 4 + 9 + 10 = 23
        assert_eq!(total_chars(&msgs), 23);
    }

    #[test]
    fn preview_truncates_in_one_pass() {
        // Single-pass truncation: up to 200 chars kept verbatim, 201+ gets an
        // ellipsis; the exact boundary (MAX) must NOT add one.
        assert_eq!(preview("hi"), "hi");
        let exact: String = "b".repeat(200);
        let p = preview(&exact);
        assert_eq!(p.chars().count(), 200);
        assert!(!p.ends_with('…'));
        let over = preview(&"a".repeat(500));
        assert!(over.ends_with('…'));
        assert_eq!(over.chars().count(), 201); // 200 + ellipsis
    }

    // ── agent-loop invariants (driven by the in-process MockProvider) ─────
    //
    // These pin down the structural rules the loop relies on — the ones that
    // used to be covered only by hand-run Python mocks. Each names the
    // invariant it guards so a future change that breaks it fails loudly.

    /// A minimal Config built from JSON (no file, no API-key lookup — the
    /// agent never calls `api_key()`; the test wires a MockProvider directly).
    fn cfg_with(max_turns: u32, timeout_secs: u64, compress: usize) -> Config {
        let mut c = serde_json::from_str::<Config>(
            r#"{"provider":"openai","api_key_env":"MOCK_API_KEY","model":"m","inject_environment":false}"#,
        )
        .expect("valid test config");
        c.limits.max_turns = max_turns;
        c.limits.timeout_secs = timeout_secs;
        c.limits.compress_threshold_chars = compress;
        c
    }

    fn test_ctx() -> ToolCtx {
        ToolCtx {
            workdir: std::env::current_dir().expect("cwd"),
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

    /// A no-op tool that can also (a) record that it ran, and (b) flip a
    /// shared cancel flag mid-run — to exercise the batch-cancel path.
    struct EchoTool {
        name: String,
        ran: Option<Arc<AtomicBool>>,
        cancel_on_run: Option<Arc<AtomicBool>>,
    }
    impl EchoTool {
        fn new(name: &str) -> Self {
            Self { name: name.into(), ran: None, cancel_on_run: None }
        }
        fn tracking(name: &str, ran: Arc<AtomicBool>) -> Self {
            Self { name: name.into(), ran: Some(ran), cancel_on_run: None }
        }
        fn cancel_on_run(name: &str, flag: Arc<AtomicBool>) -> Self {
            Self { name: name.into(), ran: None, cancel_on_run: Some(flag) }
        }
    }
    impl Tool for EchoTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: self.name.clone(),
                description: "test echo tool".into(),
                schema: json!({"type": "object", "properties": {}}),
            }
        }
        fn run(&self, _input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
            if let Some(r) = &self.ran {
                r.store(true, Ordering::Relaxed);
            }
            if let Some(c) = &self.cancel_on_run {
                c.store(true, Ordering::Relaxed);
            }
            Ok(ToolOutput::text("ok"))
        }
    }

    /// A tool that records whether `set_shutdown` was called on it, and how
    /// many times. Guards the P1 audit fix: a per-turn `set_cancel` must NOT
    /// propagate to tools (arming MCP teardown), only `set_shutdown` may.
    struct ShutdownTrackingTool {
        shutdown_calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Tool for ShutdownTrackingTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: "track".into(),
                description: "test tool recording set_shutdown calls".into(),
                schema: json!({"type": "object", "properties": {}}),
            }
        }
        fn run(&self, _input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
            Ok(ToolOutput::text("ok"))
        }
        fn set_shutdown(
            &mut self,
            _shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        ) {
            self.shutdown_calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A tool that bills synthetic usage to the parent run — pins that
    /// `ToolOutput::usage` folds into the agent's totals and emits a Usage
    /// event. This is the path the `task` sub-agent relies on for honest cost.
    struct BillTool;
    impl Tool for BillTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: "bill".into(),
                description: "test usage-billing tool".into(),
                schema: json!({"type": "object", "properties": {}}),
            }
        }
        fn run(&self, _input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
            Ok(ToolOutput {
                content: "sub-work done".into(),
                image: None,
                is_error: false,
                usage: Some(crate::provider::Usage {
                    input_tokens: 111,
                    output_tokens: 22,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                }),
                meta: None,
            })
        }
    }

    /// A tool that streams synthetic sub-agent events into the parent run via
    /// `run_with_emit` — pins that the tool loop routes the `emit` closure
    /// through to the parent's event sink (the whole point of child-event
    /// visibility for the `task` tool). Mirrors how `SubAgent` forwards its
    /// child's events wrapped as `AgentEvent::SubAgent{..}`.
    struct EmitTool;
    impl Tool for EmitTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: "emit".into(),
                description: "test tool that forwards sub-agent events".into(),
                schema: json!({"type": "object", "properties": {}}),
            }
        }
        fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
            self.run_with_emit(input, ctx, &mut |_| {})
        }
        fn run_with_emit(
            &self,
            _input: &Value,
            _ctx: &ToolCtx,
            emit: &mut dyn FnMut(AgentEvent),
        ) -> Result<ToolOutput> {
            emit(AgentEvent::SubAgent {
                name: "scout".into(),
                event: Box::new(AgentEvent::TextDelta("looking".into())),
            });
            emit(AgentEvent::SubAgent {
                name: "scout".into(),
                event: Box::new(AgentEvent::TextDelta(" found".into())),
            });
            Ok(ToolOutput::text("scout done"))
        }
    }

    /// A stand-in for the real `task` sub-agent tool. It reports
    /// `ToolConcurrency::Parallel` — THAT marker, not its name, is what routes
    /// it to the parallel batch. It reads `input["label"]` to make each call's
    /// output distinguishable, emits a name-tagged SubAgent event (so we can
    /// assert it ran), and bills usage (so we can assert parallel cost folds
    /// into the run totals). Concurrency is structural — we assert functional
    /// correctness, NOT timing (which would flake).
    struct TaskTool;
    impl Tool for TaskTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: "task".into(),
                description: "test stand-in for the task sub-agent tool".into(),
                schema: json!({"type": "object", "properties": {"label": {"type": "string"}}}),
            }
        }
        fn concurrency(&self) -> ToolConcurrency {
            ToolConcurrency::Parallel
        }
        fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
            self.run_with_emit(input, ctx, &mut |_| {})
        }
        fn run_with_emit(
            &self,
            input: &Value,
            _ctx: &ToolCtx,
            emit: &mut dyn FnMut(AgentEvent),
        ) -> Result<ToolOutput> {
            let label = input["label"].as_str().unwrap_or("?").to_string();
            emit(AgentEvent::SubAgent {
                name: label.clone(),
                event: Box::new(AgentEvent::TextDelta(format!("work-{label}"))),
            });
            Ok(ToolOutput {
                content: format!("out-{label}"),
                image: None,
                is_error: false,
                usage: Some(crate::provider::Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                }),
                meta: None,
            })
        }
    }

    /// Like `TaskTool` but each call sleeps, so a timing assertion can tell real
    /// parallelism (thread::scope fan-out) from a serialization regression: two
    /// calls overlap → ~1× the sleep; serialized → ~2×. Name, sleep length, and
    /// concurrency class are configurable so the same stub doubles as the
    /// built-in `task` shape (`SlowTool::task`) and as an arbitrary third-party
    /// tool under either class (`SlowTool::custom`). Used only by the
    /// timing-aware tests — every other concurrency test asserts function only
    /// (see TaskTool).
    struct SlowTool {
        name: &'static str,
        ms: u64,
        concurrency: ToolConcurrency,
    }
    impl SlowTool {
        /// The built-in `task` shape: named "task", 500ms, Parallel.
        fn task() -> Self {
            Self { name: "task", ms: 500, concurrency: ToolConcurrency::Parallel }
        }
        /// A third-party tool shape: arbitrary name, sleep, and class.
        fn custom(name: &'static str, ms: u64, concurrency: ToolConcurrency) -> Self {
            Self { name, ms, concurrency }
        }
    }
    impl Tool for SlowTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: self.name.into(),
                description: "test stand-in that sleeps, to assert real parallelism".into(),
                schema: json!({"type": "object", "properties": {"label": {"type": "string"}}}),
            }
        }
        fn concurrency(&self) -> ToolConcurrency {
            self.concurrency
        }
        fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
            self.run_with_emit(input, ctx, &mut |_| {})
        }
        fn run_with_emit(
            &self,
            input: &Value,
            _ctx: &ToolCtx,
            _emit: &mut dyn FnMut(AgentEvent),
        ) -> Result<ToolOutput> {
            let label = input["label"].as_str().unwrap_or("?").to_string();
            std::thread::sleep(std::time::Duration::from_millis(self.ms));
            Ok(ToolOutput::text(format!("slow-{label}")))
        }
    }

    #[test]
    fn tool_reported_usage_folds_into_run_totals_and_events() {
        // A tool reporting usage (the `task` sub-agent path) must fold into the
        // run's token totals AND emit its own AgentEvent::Usage, alongside each
        // turn's streamed usage. Pins the honest-cost fold the sub-agent relies
        // on — touched the core tool-result path, so this stays green.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("t", "bill", json!({}), mock::mk_usage(50, 7)),
            // Turn 2 is the sole cache contributor, so cache totals are unambiguous.
            mock::resp_text("final", mock::mk_usage_cache(0, 0, 5, 9)),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(BillTool)],
            vec![user_text("go")],
            test_ctx(),
        );
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        // Streamed turn usage (50↑/7↓, then 0/0) + the tool's billed (111↑/22↓).
        assert_eq!(o.input_tokens, 50 + 111);
        assert_eq!(o.output_tokens, 7 + 22);
        // Cache only from turn 2 (5 write / 9 read); the tool billed none.
        assert_eq!(o.cache_creation_input_tokens, 5);
        assert_eq!(o.cache_read_input_tokens, 9);
        // The tool's billed usage surfaced as its own Usage event (111/22).
        let usage_events: Vec<(u64, u64)> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Usage { input_tokens, output_tokens, .. } => Some((*input_tokens, *output_tokens)),
                _ => None,
            })
            .collect();
        assert!(
            usage_events.contains(&(111, 22)),
            "tool-reported usage must emit a Usage event; got {usage_events:?}"
        );
    }

    #[test]
    fn tool_emitted_subagent_events_reach_parent_stream() {
        // A tool overriding run_with_emit (the `task` path) forwards its
        // AgentEvent::SubAgent{..} into the parent's run callback, tagged with
        // the sub-agent's name — that identity is what disambiguates parallel
        // sub-agents. Pins the loop's routing of the `emit` closure, which the
        // sequential v1 changed from a plain `tool.run(...)` call.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("t", "emit", json!({}), mock::mk_usage(1, 1)),
            mock::resp_text("final", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EmitTool)],
            vec![user_text("go")],
            test_ctx(),
        );
        let mut events = Vec::new();
        agent.run(&mut |e| events.push(e)).unwrap();
        // Collect forwarded sub-agent text deltas, grouped by name.
        let forwarded: Vec<(String, String)> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::SubAgent { name, event } => match &**event {
                    AgentEvent::TextDelta(t) => Some((name.clone(), t.clone())),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert!(
            forwarded.iter().all(|(n, _)| n == "scout"),
            "every forwarded event must be tagged scout: {forwarded:?}"
        );
        let text: String = forwarded.into_iter().map(|(_, t)| t).collect();
        assert_eq!(text, "looking found", "both deltas forwarded in order");
    }

    #[test]
    fn parallel_task_calls_both_run_and_keep_order_with_usage_folded() {
        // One turn with TWO `task` calls (labels a/b). The parallel batch must
        // run BOTH (assert via the forwarded SubAgent events), keep results in
        // the ORIGINAL block order despite concurrent execution (N:N + ordered
        // records), and fold each task's billed usage into the run totals.
        // We assert functional correctness only — concurrency is structural
        // (thread::scope), never timing.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![MockTurn::Ok(ChatResponse {
            blocks: vec![
                Block::ToolUse {
                    id: "a".into(),
                    name: "task".into(),
                    input: json!({"label": "a"}),
                },
                Block::ToolUse {
                    id: "b".into(),
                    name: "task".into(),
                    input: json!({"label": "b"}),
                },
            ],
            stop_reason: "tool_use".into(),
            usage: None,
        }),
        // Turn 2 reports no usage so the only input tokens are the two tasks'.
        mock::resp_text("final", None)]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(TaskTool)],
            vec![user_text("go")],
            test_ctx(),
        );
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();

        // Both tasks ran — each forwarded a SubAgent event tagged with its label.
        let tags: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::SubAgent { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert!(tags.contains(&"a".into()) && tags.contains(&"b".into()), "both ran: {tags:?}");

        // Both tasks' ToolCall events still fire (now emitted from the batch
        // block after the gate, not the inline pass) — in block order a, b.
        let toolcalls: Vec<&Value> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolCall { name, input } if name == "task" => Some(input),
                _ => None,
            })
            .collect();
        assert_eq!(toolcalls.len(), 2, "both task ToolCalls must fire: {toolcalls:?}");
        assert_eq!(toolcalls[0]["label"], "a");
        assert_eq!(toolcalls[1]["label"], "b");

        // Results assembled in ORIGINAL block order (a before b), not completion
        // order — records must stay coherent with the tool_use sequence.
        assert_eq!(o.tool_calls.len(), 2);
        assert_eq!(o.tool_calls[0].output, "out-a");
        assert_eq!(o.tool_calls[1].output, "out-b");
        // (N:N result pairing + synthesis is pinned by the cancel test below,
        // where the run stops right after the batch so the trailing message is
        // the tool_results; here turn 2 produces the assistant's final text.)

        // Each task billed 100 input tokens → 200 total (turn usage was None/0).
        assert_eq!(o.input_tokens, 200);
        assert_eq!(o.output_tokens, 20);
    }

    #[test]
    fn parallel_task_calls_actually_overlap_in_wall_clock() {
        // One of the suite's few timing assertions. Two `task` calls each sleep
        // 500ms; real parallelism (thread::scope fan-out) finishes in ~1×
        // (≈500ms + overhead), a serialization regression (a coarse lock around
        // run_tool, or the scope replaced by a sequential loop) in ~2×
        // (≈1000ms). The 900ms threshold cleanly separates them with margin for
        // CI scheduler jitter. Without this, the functional-only tests above
        // stay green even if the batch is silently serialized — which is exactly
        // why none of them assert timing.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            MockTurn::Ok(ChatResponse {
                blocks: vec![
                    Block::ToolUse {
                        id: "a".into(),
                        name: "task".into(),
                        input: json!({"label": "a"}),
                    },
                    Block::ToolUse {
                        id: "b".into(),
                        name: "task".into(),
                        input: json!({"label": "b"}),
                    },
                ],
                stop_reason: "tool_use".into(),
                usage: None,
            }),
            mock::resp_text("final", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(SlowTool::task())],
            vec![user_text("go")],
            test_ctx(),
        );
        let start = std::time::Instant::now();
        let o = agent.run(&mut |_| {}).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(o.tool_calls.len(), 2);
        assert!(
            elapsed < std::time::Duration::from_millis(900),
            "two 500ms task calls took {elapsed:?} — expected parallel (~500ms); \
             a serialization regression would take ~1000ms"
        );
    }

    #[test]
    fn third_party_parallel_tool_overlaps_by_capability_not_name() {
        // Batch routing keys off Tool::concurrency(), NOT the name "task": a
        // custom-named tool opting into Parallel gets the identical fan-out.
        // Two 500ms calls of a tool named "worker" must overlap (~1×); a
        // regression that routes by name (or serializes the batch) takes ~2×.
        // The mirror of parallel_task_calls_actually_overlap_in_wall_clock for
        // the capability split — together they pin both halves of the routing.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            MockTurn::Ok(ChatResponse {
                blocks: vec![
                    Block::ToolUse {
                        id: "a".into(),
                        name: "worker".into(),
                        input: json!({"label": "a"}),
                    },
                    Block::ToolUse {
                        id: "b".into(),
                        name: "worker".into(),
                        input: json!({"label": "b"}),
                    },
                ],
                stop_reason: "tool_use".into(),
                usage: None,
            }),
            mock::resp_text("final", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(SlowTool::custom("worker", 500, ToolConcurrency::Parallel))],
            vec![user_text("go")],
            test_ctx(),
        );
        let start = std::time::Instant::now();
        let o = agent.run(&mut |_| {}).unwrap();
        let elapsed = start.elapsed();
        // Results still pair N:N in block order, despite the custom name.
        assert_eq!(o.tool_calls.len(), 2);
        assert_eq!(o.tool_calls[0].output, "slow-a");
        assert_eq!(o.tool_calls[1].output, "slow-b");
        assert!(
            elapsed < std::time::Duration::from_millis(900),
            "two 500ms parallel-capable calls took {elapsed:?} — a name-based \
             router would run them inline (~1000ms)"
        );
    }

    #[test]
    fn sequential_tool_calls_stay_inline_and_serialized() {
        // The other half of the capability split: a tool reporting Sequential
        // (the default class) must NOT be deferred to the parallel batch even
        // with multiple calls in one turn — over-fan-out would race leaf tools
        // on shell/file. Two 400ms calls run inline back-to-back (~2×), and
        // results still pair N:N in block order.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            MockTurn::Ok(ChatResponse {
                blocks: vec![
                    Block::ToolUse {
                        id: "a".into(),
                        name: "slowseq".into(),
                        input: json!({"label": "a"}),
                    },
                    Block::ToolUse {
                        id: "b".into(),
                        name: "slowseq".into(),
                        input: json!({"label": "b"}),
                    },
                ],
                stop_reason: "tool_use".into(),
                usage: None,
            }),
            mock::resp_text("final", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(SlowTool::custom("slowseq", 400, ToolConcurrency::Sequential))],
            vec![user_text("go")],
            test_ctx(),
        );
        let start = std::time::Instant::now();
        let o = agent.run(&mut |_| {}).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(o.tool_calls.len(), 2);
        assert_eq!(o.tool_calls[0].output, "slow-a");
        assert_eq!(o.tool_calls[1].output, "slow-b");
        // Serialized ≥800ms in theory; 700ms leaves jitter margin while staying
        // far above the ~400ms a (wrong) parallel batch would take.
        assert!(
            elapsed >= std::time::Duration::from_millis(700),
            "two 400ms sequential calls took {elapsed:?} — expected inline \
             serialization (~800ms), not the parallel batch (~400ms)"
        );
    }

    #[test]
    fn mixed_turn_assembles_results_in_block_order() {
        // One turn mixing the two classes — [Sequential, Parallel, Sequential]:
        // the leaves run inline first (block order), the Parallel call fans out
        // after, yet the assembled results/records must land in ORIGINAL block
        // order, the middle one between the two leaves.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            MockTurn::Ok(ChatResponse {
                blocks: vec![
                    Block::ToolUse { id: "e1".into(), name: "first".into(), input: json!({}) },
                    Block::ToolUse {
                        id: "p".into(),
                        name: "task".into(),
                        input: json!({"label": "m"}),
                    },
                    Block::ToolUse { id: "e2".into(), name: "second".into(), input: json!({}) },
                ],
                stop_reason: "tool_use".into(),
                usage: None,
            }),
            mock::resp_text("final", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![
                Box::new(EchoTool::new("first")),
                Box::new(TaskTool),
                Box::new(EchoTool::new("second")),
            ],
            vec![user_text("go")],
            test_ctx(),
        );
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.tool_calls.len(), 3);
        assert_eq!(o.tool_calls[0].name, "first");
        assert_eq!(o.tool_calls[0].output, "ok");
        assert_eq!(o.tool_calls[1].name, "task");
        assert_eq!(o.tool_calls[1].output, "out-m");
        assert_eq!(o.tool_calls[2].name, "second");
        assert_eq!(o.tool_calls[2].output, "ok");
        // records and the history tool_result message are assembled by the same
        // block-order loop over the slots, so ordered records pin the assembly
        // (N:N history pairing itself is pinned by the cancel test above, which
        // stops right at the tool_result message).
    }

    #[test]
    fn cancel_gates_the_parallel_task_batch_and_keeps_n_n_pairing() {
        // A leaf tool ("trip") flips cancel mid-batch; the FOLLOWING `task` call
        // must be gated off (never spawned) — the batch checks `batch_cancelled`
        // before fanning out. Yet the orphaned task_use still gets a synthesized
        // "[cancelled by user]" result so pairing holds (N:N), and it is NOT
        // recorded as executed.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![MockTurn::Ok(ChatResponse {
            blocks: vec![
                Block::ToolUse { id: "a".into(), name: "trip".into(), input: json!({}) },
                Block::ToolUse { id: "b".into(), name: "task".into(), input: json!({"label": "x"}) },
            ],
            stop_reason: "tool_use".into(),
            usage: None,
        })]);
        let flag = Arc::new(AtomicBool::new(false));
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![
                Box::new(EchoTool::cancel_on_run("trip", flag.clone())),
                Box::new(TaskTool),
            ],
            vec![user_text("go")],
            test_ctx(),
        );
        agent.set_cancel(flag);
        let mut saw_task_subagent = false;
        let mut saw_task_toolcall = false;
        let o = agent.run(&mut |e| match &e {
            AgentEvent::SubAgent { name, .. } if name == "x" => saw_task_subagent = true,
            AgentEvent::ToolCall { name, .. } if name == "task" => saw_task_toolcall = true,
            _ => {}
        })
        .unwrap();

        assert_eq!(o.stop_reason, "cancelled");
        // Only the leaf "trip" executed; the gated `task` is NOT recorded.
        assert_eq!(o.tool_calls.len(), 1);
        assert_eq!(o.tool_calls[0].name, "trip");
        // The task never ran (no forwarded SubAgent event for label "x").
        assert!(!saw_task_subagent, "gated task must not have spawned");
        // And it never had its ToolCall announced either — emitting it before the
        // gate would dangle a ToolCall with no matching ToolResult on the UI.
        assert!(
            !saw_task_toolcall,
            "gated task must not emit a ToolCall (would dangle without a ToolResult)"
        );
        // N:N pairing intact: trip → "ok", task → synthesized cancelled result.
        assert_eq!(
            agent.trailing_tool_result_contents(),
            vec!["ok".to_string(), "[cancelled by user]".to_string()]
        );
    }

    // ── Hooks interception layer (commit 4) ─────────────────────────────────

    /// A recording [`Hooks`] impl for the interception-layer tests. Logs every
    /// callback in call order — tagging what the contract promises: the ctx's
    /// sub-agent identity in `before_tool`, the executed output in
    /// `after_tool`, the model in `on_chat_request` — and can be pre-seeded
    /// with a per-tool `before_tool` decision (absent → Allow). Mutex-guarded
    /// like any real implementation must be — hooks may be called from several
    /// worker threads at once (sibling sub-agents).
    struct CaptureHooks {
        calls: std::sync::Mutex<Vec<String>>,
        decisions: std::sync::Mutex<std::collections::HashMap<String, ToolDecision>>,
    }
    impl CaptureHooks {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                decisions: std::sync::Mutex::new(std::collections::HashMap::new()),
            }
        }
        fn deny(tool: &str, reason: &str) -> Self {
            let h = Self::new();
            h.decisions
                .lock()
                .unwrap()
                .insert(tool.to_string(), ToolDecision::Deny(reason.to_string()));
            h
        }
        fn modify(tool: &str, replacement: Value) -> Self {
            let h = Self::new();
            h.decisions
                .lock()
                .unwrap()
                .insert(tool.to_string(), ToolDecision::Modify(replacement));
            h
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }
    impl Hooks for CaptureHooks {
        fn before_tool(&self, tool: &str, input: &Value, ctx: &ToolCtx) -> ToolDecision {
            let tag = ctx.subagent.as_deref().unwrap_or("-");
            self.calls
                .lock()
                .unwrap()
                .push(format!("before:{tool}:{input}:{tag}"));
            match self.decisions.lock().unwrap().get(tool) {
                Some(d) => d.clone(),
                None => ToolDecision::Allow,
            }
        }
        fn after_tool(&self, tool: &str, _input: &Value, output: &ToolOutput, executed: bool) {
            self.calls.lock().unwrap().push(format!(
                "after:{tool}:{executed}:{err}:{content}",
                err = output.is_error,
                content = output.content
            ));
        }
        fn on_chat_request(&self, req: &ChatRequest) {
            self.calls.lock().unwrap().push(format!("chat:{}", req.model));
        }
    }

    /// Echoes the EXECUTED input back as the output — a Modify test can then
    /// tell from the record which input actually ran (vs. the original the
    /// audit keeps).
    struct InputEchoTool {
        name: &'static str,
    }
    impl Tool for InputEchoTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: self.name.into(),
                description: "echoes the executed input".into(),
                schema: json!({"type": "object"}),
            }
        }
        fn run(&self, input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
            Ok(ToolOutput::text(input.to_string()))
        }
    }

    #[test]
    fn hooks_deny_leaf_records_error_result_and_continues() {
        // A denied Sequential call: never executed, yet RECORDED (is_error, the
        // denial text as output), its tool_result keeps N:N pairing, and the
        // run continues to the next turn. after_tool fires at decision time
        // with executed=false. Deny lands as Slot::Ran — Slot::Cancelled stays
        // reserved for cancel.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("t1", "danger", json!({"cmd": "rm -rf /"}), mock::mk_usage(5, 5)),
            mock::resp_text("final", None),
        ]);
        let ran = Arc::new(AtomicBool::new(false));
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::tracking("danger", ran.clone()))],
            vec![user_text("go")],
            test_ctx(),
        );
        let hooks = Arc::new(CaptureHooks::deny("danger", "forbidden in this deployment"));
        agent.set_hooks(hooks.clone());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();

        assert!(!ran.load(Ordering::Relaxed), "a denied tool must never execute");
        assert_eq!(o.tool_calls.len(), 1);
        assert!(o.tool_calls[0].is_error);
        assert_eq!(o.tool_calls[0].output, "forbidden in this deployment");
        // The denial text is what the model sees; it then produced the final turn.
        assert_eq!(o.final_text, "final");
        // The ToolResult event reports the error.
        let errs: Vec<bool> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult { is_error, .. } => Some(*is_error),
                _ => None,
            })
            .collect();
        assert_eq!(errs, vec![true]);
        // Exact hook sequence: chat(turn1) → before → after(denied) → chat(turn2).
        let expected: Vec<String> = [
            "chat:m",
            r#"before:danger:{"cmd":"rm -rf /"}:-"#,
            "after:danger:false:true:forbidden in this deployment",
            "chat:m",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(hooks.calls(), expected);
    }

    #[test]
    fn hooks_modify_leaf_executes_replacement_but_audit_keeps_original() {
        // Modify: the tool RUNS with the replacement input; the audit trail
        // (recorded input + announced ToolCall event) keeps the model's
        // original — the log answers "what did the model ask for".
        let cfg = cfg_with(5, 0, 0);
        let original = json!({"path": "/etc/passwd"});
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("t1", "echo", original.clone(), mock::mk_usage(1, 1)),
            mock::resp_text("final", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(InputEchoTool { name: "echo" })],
            vec![user_text("go")],
            test_ctx(),
        );
        let hooks = Arc::new(CaptureHooks::modify("echo", json!({"path": "safe.txt"})));
        agent.set_hooks(hooks.clone());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();

        // Executed input was the replacement...
        assert_eq!(o.tool_calls[0].output, r#"{"path":"safe.txt"}"#);
        assert!(!o.tool_calls[0].is_error);
        // ...but the audit keeps the original.
        assert_eq!(o.tool_calls[0].input, original);
        let announced: Vec<Value> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolCall { name, input } if name == "echo" => Some(input.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(announced, vec![original]);
    }

    #[test]
    fn hooks_allow_is_equivalent_to_no_hooks() {
        // An all-Allow layer must be observably a no-op: same outcome fields,
        // same event stream as no layer at all (elapsed aside). Guards against
        // accidental drift from merely wiring the hook call sites.
        let script = || {
            vec![
                mock::resp_tool("t1", "echo", json!({"v": 1}), mock::mk_usage(7, 3)),
                mock::resp_text("done", mock::mk_usage(2, 1)),
            ]
        };
        let build = |hooks: Option<Arc<CaptureHooks>>| {
            let cfg = cfg_with(5, 0, 0);
            let (prov, _) = MockProvider::new(script());
            let mut agent = Agent::new(
                &cfg,
                Box::new(prov),
                vec![Box::new(InputEchoTool { name: "echo" })],
                vec![user_text("go")],
                test_ctx(),
            );
            if let Some(h) = hooks {
                agent.set_hooks(h);
            }
            agent
        };
        let mut evs_plain = Vec::new();
        let o_plain = build(None).run(&mut |e| evs_plain.push(e)).unwrap();
        let mut evs_hooked = Vec::new();
        let o_hooked = build(Some(Arc::new(CaptureHooks::new())))
            .run(&mut |e| evs_hooked.push(e))
            .unwrap();

        assert_eq!(o_hooked.final_text, o_plain.final_text);
        assert_eq!(o_hooked.turns, o_plain.turns);
        assert_eq!(o_hooked.stop_reason, o_plain.stop_reason);
        assert_eq!(
            format!("{:?}", o_hooked.tool_calls),
            format!("{:?}", o_plain.tool_calls)
        );
        assert_eq!(o_hooked.input_tokens, o_plain.input_tokens);
        assert_eq!(o_hooked.output_tokens, o_plain.output_tokens);
        assert_eq!(
            o_hooked.cache_creation_input_tokens,
            o_plain.cache_creation_input_tokens
        );
        assert_eq!(
            o_hooked.cache_read_input_tokens,
            o_plain.cache_read_input_tokens
        );
        assert_eq!(format!("{evs_hooked:?}"), format!("{evs_plain:?}"));
    }

    #[test]
    fn hooks_deny_task_call_never_spawns_and_bills_nothing() {
        // A denied Parallel call: the batch gate refuses it BEFORE any worker
        // spawns — no SubAgent events, no billed usage — yet it's recorded
        // is_error (honest audit) and the run continues.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("t1", "task", json!({"label": "a"}), mock::mk_usage(5, 5)),
            mock::resp_text("final", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(TaskTool)],
            vec![user_text("go")],
            test_ctx(),
        );
        let hooks = Arc::new(CaptureHooks::deny("task", "no sub-agents in this deployment"));
        agent.set_hooks(hooks.clone());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();

        assert!(
            events.iter().all(|e| !matches!(e, AgentEvent::SubAgent { .. })),
            "a denied task must never spawn a sub-agent"
        );
        assert_eq!(o.tool_calls.len(), 1);
        assert!(o.tool_calls[0].is_error);
        // Only turn-1 usage billed — the denied task's 100/10 never ran.
        assert_eq!(o.input_tokens, 5);
        assert_eq!(o.output_tokens, 5);
    }

    #[test]
    fn hooks_modify_task_input_reaches_the_worker() {
        // Modify on a Parallel call: the spawned worker EXECUTES the
        // replacement input, while the audit trail keeps the original.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("t1", "task", json!({"label": "a"}), mock::mk_usage(5, 5)),
            mock::resp_text("final", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(TaskTool)],
            vec![user_text("go")],
            test_ctx(),
        );
        let hooks = Arc::new(CaptureHooks::modify("task", json!({"label": "b"})));
        agent.set_hooks(hooks.clone());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();

        // The worker saw the replacement (TaskTool derives output + embedded
        // event tag from the executed input).
        assert_eq!(o.tool_calls[0].output, "out-b");
        let tags: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::SubAgent { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(tags, vec!["b".to_string()]);
        // Audit keeps the original.
        assert_eq!(o.tool_calls[0].input, json!({"label": "a"}));
    }

    #[test]
    fn hooks_see_subagent_tag_when_gating_inside_a_subagent_ctx() {
        // A child agent (ctx.subagent = Some) gates its own leaf tools through
        // the inherited layer, and before_tool sees the sub-agent identity via
        // ctx. (A full child-agent e2e is impossible offline — the child
        // builds its own provider internally — so this simulates the child
        // side: an agent whose ctx is tagged exactly like child_ctx() tags it.)
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("t1", "leaf", json!({}), mock::mk_usage(1, 1)),
            mock::resp_text("final", None),
        ]);
        let mut ctx = test_ctx();
        ctx.subagent = Some("scout".into());
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("leaf"))],
            vec![user_text("go")],
            ctx,
        );
        let hooks = Arc::new(CaptureHooks::deny("leaf", "children don't shell out"));
        agent.set_hooks(hooks.clone());
        agent.run(&mut |_| {}).unwrap();
        let calls = hooks.calls();
        assert!(
            calls.contains(&"before:leaf:{}:scout".to_string()),
            "before_tool must see the sub-agent tag: {calls:?}"
        );
    }

    #[test]
    fn hooks_on_chat_request_fires_once_per_main_turn() {
        // Three main turns (tool, tool, text) → exactly three on_chat_request
        // observations, one per outgoing request. Compression is disabled
        // (threshold 0) so no summary call can sneak in — and if one ever ran,
        // it must NOT be surfaced (main-turns-only hook).
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("t1", "echo", json!({}), mock::mk_usage(1, 1)),
            mock::resp_tool("t2", "echo", json!({}), mock::mk_usage(1, 1)),
            mock::resp_text("done", mock::mk_usage(1, 1)),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(InputEchoTool { name: "echo" })],
            vec![user_text("go")],
            test_ctx(),
        );
        let hooks = Arc::new(CaptureHooks::new());
        agent.set_hooks(hooks.clone());
        agent.run(&mut |_| {}).unwrap();
        let calls = hooks.calls();
        let chats: Vec<&str> = calls.iter().filter_map(|c| c.strip_prefix("chat:")).collect();
        assert_eq!(chats.len(), 3, "one on_chat_request per main turn: {calls:?}");
    }

    #[test]
    fn hooks_on_chat_request_skips_the_compression_summary_call() {
        // Companion to the per-turn test above, covering the case it can't:
        // compression ENABLED. The run makes TWO provider calls — the internal
        // summary request plus the main turn — but on_chat_request must fire
        // only for the MAIN one (summary_call is deliberately not hooked; see
        // Hooks doc, "Scope"). Without this test a future hook wired into
        // summary_call would pass the whole suite (every other hooks test runs
        // with compression disabled). Setup mirrors
        // compress_folds_history_and_emits_event so compression provably runs.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", None), // summarize_prefix call (NOT hooked)
            mock::resp_text("final answer", None), // the only main turn
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let hooks = Arc::new(CaptureHooks::new());
        agent.set_hooks(hooks.clone());
        let mut events = Vec::new();
        agent.run(&mut |e| events.push(e)).unwrap();
        // Compression really ran (summary call happened)...
        assert_eq!(state.calls(), 2, "summary + main = two provider calls");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Compressed { .. })),
            "compression must actually fire for this test to mean anything"
        );
        // ...but hooks saw exactly ONE chat request — the main turn's.
        let calls = hooks.calls();
        let chats: Vec<&str> = calls.iter().filter_map(|c| c.strip_prefix("chat:")).collect();
        assert_eq!(
            chats.len(),
            1,
            "summary_call must not surface to on_chat_request: {calls:?}"
        );
    }

    #[test]
    fn hooks_parallel_batch_gates_all_calls_in_block_order() {
        // Three Parallel calls in one turn: before_tool fires for each, in
        // block order, before execution; after_tool fires for each after the
        // batch joins, again in block order (join order == spawn order). One
        // chat observation frames each turn.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            MockTurn::Ok(ChatResponse {
                blocks: vec![
                    Block::ToolUse { id: "a".into(), name: "task".into(), input: json!({"label": "a"}) },
                    Block::ToolUse { id: "b".into(), name: "task".into(), input: json!({"label": "b"}) },
                    Block::ToolUse { id: "c".into(), name: "task".into(), input: json!({"label": "c"}) },
                ],
                stop_reason: "tool_use".into(),
                usage: None,
            }),
            mock::resp_text("final", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(TaskTool)],
            vec![user_text("go")],
            test_ctx(),
        );
        let hooks = Arc::new(CaptureHooks::new());
        agent.set_hooks(hooks.clone());
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.tool_calls.len(), 3);
        let expected: Vec<String> = [
            "chat:m",
            r#"before:task:{"label":"a"}:-"#,
            r#"before:task:{"label":"b"}:-"#,
            r#"before:task:{"label":"c"}:-"#,
            "after:task:true:false:out-a",
            "after:task:true:false:out-b",
            "after:task:true:false:out-c",
            "chat:m",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(hooks.calls(), expected);
    }

    #[test]
    fn hooks_mixed_turn_gates_leaves_first_then_batch_in_block_order() {
        // A MIXED turn pins the documented non-global gate order: every
        // Sequential call passes its gate DURING the inline pass
        // (gate-then-run, so its after fires before any parallel gate is even
        // consulted), then the batch's gates run serially in parallel-block
        // order before any worker spawns, and the afters fire after the join.
        // With block order [task(a), echo, task(b)] the echo gate+after land
        // BEFORE both task gates — global-block-order gating (the inline pass
        // consulting parallel calls too) would reorder them and fail this.
        // (The pure paths are pinned by the deny/modify + parallel-batch
        // tests; this covers the mix the doc warns about. Records still
        // assemble in BLOCK order — pinned by
        // mixed_turn_assembles_results_in_block_order — the GATE order is
        // deliberately different.)
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            MockTurn::Ok(ChatResponse {
                blocks: vec![
                    Block::ToolUse { id: "a".into(), name: "task".into(), input: json!({"label": "a"}) },
                    Block::ToolUse { id: "e".into(), name: "echo".into(), input: json!({}) },
                    Block::ToolUse { id: "b".into(), name: "task".into(), input: json!({"label": "b"}) },
                ],
                stop_reason: "tool_use".into(),
                usage: None,
            }),
            mock::resp_text("final", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(TaskTool), Box::new(EchoTool::new("echo"))],
            vec![user_text("go")],
            test_ctx(),
        );
        let hooks = Arc::new(CaptureHooks::new());
        agent.set_hooks(hooks.clone());
        let o = agent.run(&mut |_| {}).unwrap();
        // Records assemble in block order regardless of gate order.
        let names: Vec<&str> = o.tool_calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["task", "echo", "task"]);
        let expected: Vec<String> = [
            "chat:m",
            "before:echo:{}:-",
            "after:echo:true:false:ok",
            r#"before:task:{"label":"a"}:-"#,
            r#"before:task:{"label":"b"}:-"#,
            "after:task:true:false:out-a",
            "after:task:true:false:out-b",
            "chat:m",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(hooks.calls(), expected);
    }

    #[test]
    fn run_resets_stale_cancel_so_first_turn_is_not_cancelled() {
        // A cancel left set from a previous run must be cleared at entry —
        // cancel is level-triggered and never auto-clears elsewhere, so
        // without the reset the first turn breaks "cancelled" before chat.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![mock::resp_text("done", None)]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("hi")], test_ctx());
        agent.set_cancel(Arc::new(AtomicBool::new(true))); // stale cancel
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(o.turns, 1);
    }

    #[test]
    fn streaming_request_carries_timeout_backstop_without_run_deadline() {
        // P2-7: a streaming request must ALWAYS carry a per-request timeout,
        // even when the run has no deadline (`timeout_secs == 0`). The
        // `sse-reader` worker thread is blocked in a reqwest `read()` it can't
        // observe cancel during, so a `None` timeout would let it linger
        // forever on a half-open connection after the caller has given up
        // (idle/cancel) — a thread + TCP connection leak. The leak backstop is
        // `STREAM_BACKSTOP` whenever the run budget is `None`.
        let cfg = cfg_with(5, 0, 0); // timeout_secs=0 → no run deadline
        let (prov, state) = MockProvider::new(vec![mock::resp_text("done", None)]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("hi")], test_ctx());
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        let caps = state.captured();
        assert!(!caps.is_empty(), "provider must have been called");
        assert_eq!(
            caps[0].per_request_timeout,
            Some(STREAM_BACKSTOP),
            "streaming request without a run deadline must still set the leak backstop"
        );
    }

    #[test]
    fn run_as_subagent_does_not_reset_a_set_cancel_flag() {
        // The `task` sub-agent's child shares the parent's cancel Arc and runs
        // via run_as_subagent, which — unlike the top-level run — must NOT reset
        // the flag at entry. If it did, a sibling starting later in a parallel
        // `task` batch would clear a Ctrl-C another child (or the parent) is
        // mid-responding to, dropping the cancel for the whole batch (the race
        // CLAUDE.md records batch::run avoiding by not sharing the flag). Pin
        // the invariant directly: with the flag already set, the sub-agent run
        // honours it (stop_reason "cancelled") and leaves the flag set, rather
        // than resetting it and proceeding.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![mock::resp_text("x", None)]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("hi")], test_ctx());
        let flag = Arc::new(AtomicBool::new(true)); // already cancelled
        agent.set_cancel(flag.clone());
        let o = agent.run_as_subagent(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "cancelled", "sub-agent must honour a set flag, not reset it");
        assert!(
            flag.load(Ordering::Relaxed),
            "entry must not have cleared the shared flag"
        );
    }

    #[test]
    fn set_cancel_does_not_arm_tool_teardown_only_set_shutdown_does() {
        // P1 audit fix: the per-turn `cancel` flag and the session-end
        // `shutdown` flag are now SEPARATE. `set_cancel` (TUI Esc / headless
        // Ctrl-C) is recoverable — the next turn reuses the same MCP server —
        // so it must NOT arm teardown on tools. Only `set_shutdown` (session
        // end) may. Previously set_cancel propagated to the MCP reaper, so a
        // single Esc killed the server the next turn couldn't reuse. Pin the
        // wiring so that regression cannot return silently.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![mock::resp_text("x", None)]);
        let shutdown_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tool = ShutdownTrackingTool {
            shutdown_calls: shutdown_calls.clone(),
        };
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(tool)],
            vec![user_text("hi")],
            test_ctx(),
        );
        // A per-turn cancel must NOT reach the tool's teardown path.
        agent.set_cancel(Arc::new(AtomicBool::new(false)));
        assert_eq!(
            shutdown_calls.load(Ordering::Relaxed),
            0,
            "set_cancel must not arm tool teardown (per-turn cancel is recoverable)"
        );
        // The session-end shutdown DOES arm teardown (once).
        agent.set_shutdown(Arc::new(AtomicBool::new(false)));
        assert_eq!(
            shutdown_calls.load(Ordering::Relaxed),
            1,
            "set_shutdown must arm tool teardown exactly once"
        );
        // A second cancel still must not double-arm or interfere.
        agent.set_cancel(Arc::new(AtomicBool::new(false)));
        assert_eq!(
            shutdown_calls.load(Ordering::Relaxed),
            1,
            "set_cancel after set_shutdown must not re-arm teardown"
        );
    }

    /// A Provider that flips a shared cancel flag as `chat()` returns, with a
    /// stop_reason NOT in the abort set ("end_turn"). Used to exercise the M8
    /// window: a Ctrl-C lands between `chat()` returning and the abort check,
    /// so the provider's own stop_reason is still "end_turn" — the loop must
    /// override it to "cancelled", or the run exits 0 (end_turn) instead of 130.
    struct CancelOnChatProvider {
        flag: Arc<AtomicBool>,
    }
    impl Provider for CancelOnChatProvider {
        fn chat(&self, _req: &ChatRequest, on_event: &mut dyn FnMut(ChatEvent)) -> Result<ChatResponse> {
            on_event(ChatEvent::TextDelta("partial".into()));
            // Simulate a Ctrl-C landing as the stream wraps up: the agent's own
            // cancel Arc flips true here — AFTER run()'s entry reset, BEFORE the
            // abort check runs. (A real cancel during the final streaming bytes
            // hits the same window: CancellableSse breaks the stream, chat()
            // returns with whatever stop_reason the provider last sent.)
            self.flag.store(true, Ordering::Relaxed);
            Ok(ChatResponse {
                blocks: vec![Block::Text("partial".into())],
                stop_reason: "end_turn".into(),
                usage: None,
            })
        }
    }

    #[test]
    fn cancel_arriving_after_chat_reports_cancelled_not_end_turn() {
        // M8: a Ctrl-C that lands in the window between chat() returning and the
        // abort check must surface stop_reason "cancelled" (→ exit 130), even
        // though the provider's own stop_reason was "end_turn". Without the
        // override the run reports "end_turn" (→ exit 0) and silently swallows
        // the cancel — a real Ctrl-C during a response's final streaming bytes
        // used to exit 0 instead of 130.
        let cfg = cfg_with(5, 0, 0);
        let flag = Arc::new(AtomicBool::new(false));
        let prov = CancelOnChatProvider { flag: flag.clone() };
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("hi")], test_ctx());
        agent.set_cancel(flag);
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "cancelled");
        // The partial text the provider streamed still surfaces to the caller.
        assert_eq!(o.final_text, "partial");
    }

    #[test]
    fn empty_assistant_turn_is_not_persisted() {
        // M10: a content-filtered / genuinely empty response (zero blocks,
        // stop_reason "end_turn") must NOT be pushed as an empty assistant
        // message — Anthropic requires each message to carry >=1 content block,
        // so an empty assistant turn makes EVERY later request 400 until the
        // session is cleared. The loop stops here without persisting the turn.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![MockTurn::Ok(ChatResponse {
            blocks: vec![],
            stop_reason: "end_turn".into(),
            usage: None,
        })]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("hi")], test_ctx());
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(o.turns, 1);
        assert_eq!(o.final_text, "");
        // No empty assistant message was pushed: history is still just the one
        // user turn (same-module access to the private `messages` field).
        assert_eq!(
            agent.messages.len(),
            1,
            "empty assistant turn must not be persisted"
        );
        assert_eq!(agent.messages[0].role, Role::User);
    }

    #[test]
    fn parallel_tool_uses_pair_with_n_tool_results() {
        // One assistant turn with 2 tool_use → the NEXT request's trailing
        // message must be a user turn carrying 2 tool_results (N:N), or a
        // strict provider rejects the orphaned tool_use with a 400.
        let cfg = cfg_with(5, 0, 0);
        let (prov, state) = MockProvider::new(vec![
            MockTurn::Ok(ChatResponse {
                blocks: vec![
                    Block::ToolUse { id: "a".into(), name: "echo".into(), input: json!({}) },
                    Block::ToolUse { id: "b".into(), name: "echo".into(), input: json!({}) },
                ],
                stop_reason: "tool_use".into(),
                usage: None,
            }),
            mock::resp_text("all done", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("echo"))],
            vec![user_text("do it")],
            test_ctx(),
        );
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(o.tool_calls.len(), 2);
        // The 2nd chat saw a trailing USER message with exactly 2 tool_results.
        let cap = state.captured();
        assert_eq!(cap.len(), 2);
        assert_eq!(cap[1].last_role, Some(Role::User));
        assert_eq!(cap[1].trailing_tool_results, 2);
    }

    #[test]
    fn cancel_mid_batch_synthesizes_results_for_remaining_tools() {
        // 2 parallel tool calls; the first tool flips cancel. The second must
        // STILL get a tool_result ("[cancelled by user]") so the assistant's
        // 2 tool_use are answered 1:1, and the run ends "cancelled".
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![MockTurn::Ok(ChatResponse {
            blocks: vec![
                Block::ToolUse { id: "a".into(), name: "echo".into(), input: json!({}) },
                Block::ToolUse { id: "b".into(), name: "echo".into(), input: json!({}) },
            ],
            stop_reason: "tool_use".into(),
            usage: None,
        })]);
        let flag = Arc::new(AtomicBool::new(false));
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::cancel_on_run("echo", flag.clone()))],
            vec![user_text("go")],
            test_ctx(),
        );
        agent.set_cancel(flag); // so cancelled() is observable; the tool flips it
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "cancelled");
        // Only the first tool actually executed; the cancelled one is NOT in
        // tool_calls (those record real executions).
        assert_eq!(o.tool_calls.len(), 1);
        assert!(!o.tool_calls[0].is_error);
        // But the message history still answers both tool_use 1:1 — the
        // cancelled tool gets a synthesized result so pairing holds.
        assert_eq!(
            agent.trailing_tool_result_contents(),
            vec!["ok".to_string(), "[cancelled by user]".to_string()]
        );
    }

    #[test]
    fn aborted_stream_does_not_execute_truncated_tool_use() {
        // stop_reason "max_tokens" with a half-delivered tool_use: the tool
        // must NOT run (executing truncated args is garbage), and no
        // tool_result is synthesized — the orphaned tool_use is dropped.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![MockTurn::Ok(ChatResponse {
            blocks: vec![
                Block::Text("partial...".into()),
                Block::ToolUse { id: "x".into(), name: "echo".into(), input: json!({"_raw": "trun"}) },
            ],
            stop_reason: "max_tokens".into(),
            usage: None,
        })]);
        let ran = Arc::new(AtomicBool::new(false));
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::tracking("echo", ran.clone()))],
            vec![user_text("go")],
            test_ctx(),
        );
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "max_tokens");
        assert!(o.tool_calls.is_empty(), "truncated tool_use must not execute");
        assert!(!ran.load(Ordering::Relaxed), "tool.run was not called");
        assert_eq!(o.final_text, "partial..."); // partial text still surfaced
    }

    #[test]
    fn max_turns_caps_the_loop() {
        // max_turns=1: exactly one chat call, then stop — even though the
        // model keeps requesting tools.
        let cfg = cfg_with(1, 0, 0);
        let (prov, state) = MockProvider::new(vec![
            mock::resp_tool("a", "echo", json!({}), None),
            mock::resp_tool("b", "echo", json!({}), None), // turn 2, never reached
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("echo"))],
            vec![user_text("loop")],
            test_ctx(),
        );
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "max_turns_reached");
        assert_eq!(o.turns, 1);
        assert_eq!(state.calls(), 1, "only one chat call within the budget");
    }

    #[test]
    fn max_total_tokens_caps_the_loop_with_budget_exceeded() {
        // max_total_tokens=100: turn 1 streams a tool_use whose usage totals
        // 110, pushing the accumulated spend past the cap. The cap is checked
        // at the next turn boundary (like timeout_secs), so turn 1 finishes
        // normally (tool runs, assistant message kept) and the run stops with
        // stop_reason "budget_exceeded" instead of starting turn 2. max_turns
        // is left high so it cannot mask the budget stop.
        let mut cfg = cfg_with(99, 0, 0);
        cfg.limits.max_total_tokens = 100;
        let (prov, state) = MockProvider::new(vec![
            mock::resp_tool("a", "echo", json!({}), mock::mk_usage(80, 30)),
            mock::resp_text("done", None), // turn 2, never reached
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("echo"))],
            vec![user_text("hi")],
            test_ctx(),
        );
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "budget_exceeded");
        assert_eq!(o.turns, 1, "turn 1 ran; the cap refused to start turn 2");
        assert_eq!(state.calls(), 1, "no second chat call once the budget was crossed");
        // The turn that crossed the cap still bills its usage honestly.
        assert_eq!(o.input_tokens, 80);
        assert_eq!(o.output_tokens, 30);
    }

    #[test]
    fn run_installs_a_fleet_budget_counter_and_resets_it_per_run() {
        // With max_total_tokens set, a top-level run publishes a shared spend
        // counter to the tool ctx (the channel `task` children inherit), and
        // every direct chat bills into it. A repeat run on the same Agent
        // (TUI follow-up / harness retry) gets a FRESH counter — usage totals
        // also reset per run, so the fleet budget is per-run, not lifetime.
        let mut cfg = cfg_with(99, 0, 0);
        cfg.limits.max_total_tokens = 1_000;
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("a", "echo", json!({}), mock::mk_usage(30, 10)),
            mock::resp_text("done", mock::mk_usage(20, 5)),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("echo"))],
            vec![user_text("hi")],
            test_ctx(),
        );
        agent.run(&mut |_| {}).unwrap();
        let counter = agent.ctx.budget.clone().expect("counter installed by run");
        assert_eq!(counter.load(Ordering::Relaxed), 65, "both chats billed: 40 + 25");
        // Second run: fresh provider script, fresh spend — the counter must
        // NOT still carry the previous run's 65.
        let (prov2, _) = MockProvider::new(vec![mock::resp_text("again", mock::mk_usage(7, 3))]);
        agent.provider = Box::new(prov2);
        agent.run(&mut |_| {}).unwrap();
        let counter2 = agent.ctx.budget.clone().expect("counter re-installed");
        assert_eq!(counter2.load(Ordering::Relaxed), 10, "fresh counter per run");
        assert!(!(
            std::sync::Arc::ptr_eq(&counter, &counter2)
        ), "a new run must replace, not reuse, the counter");
    }

    #[test]
    fn tool_billed_usage_does_not_double_count_in_the_fleet_counter() {
        // The tool-billed fold (a `task` child's ToolOutput.usage landing in
        // the parent's totals) must stay OUT of the shared counter: the child
        // already shared its direct chat spend while it ran, so adding the
        // fold here too would double-count and halve the fleet's effective
        // budget. Own totals (o.input_tokens) still include the billed usage.
        let mut cfg = cfg_with(99, 0, 0);
        cfg.limits.max_total_tokens = 1_000;
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("a", "bill", json!({}), mock::mk_usage(40, 0)),
            mock::resp_text("done", mock::mk_usage(10, 0)),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(BillTool)],
            vec![user_text("hi")],
            test_ctx(),
        );
        let o = agent.run(&mut |_| {}).unwrap();
        // Accounting total = both chats (40+10) + the tool's billed 111.
        assert_eq!(o.input_tokens, 161);
        // Fleet counter = direct chats ONLY (40+10); BillTool's 111 is a
        // child-spend stand-in that "already counted itself" elsewhere.
        let counter = agent.ctx.budget.clone().expect("counter installed");
        assert_eq!(counter.load(Ordering::Relaxed), 50);
    }

    #[test]
    fn max_total_tokens_zero_means_no_cap() {
        // 0 is the "no cap" sentinel: the run must proceed to a normal
        // end_turn even though usage accumulates, never hitting a budget stop.
        let cfg = cfg_with(99, 0, 0); // max_total_tokens defaults to 0
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("a", "echo", json!({}), mock::mk_usage(999, 999)),
            mock::resp_text("done", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("echo"))],
            vec![user_text("hi")],
            test_ctx(),
        );
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(o.turns, 2);
    }

    #[test]
    fn timeout_zero_means_no_timeout() {
        // Guards the elapsed()>=0 trap: with timeout_secs=0 the run must
        // proceed normally instead of timing out before its first turn.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![mock::resp_text("ok", None)]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("hi")], test_ctx());
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(o.turns, 1);
    }

    #[test]
    fn timeout_with_subsecond_budget_stops_cleanly() {
        // timeout_secs=1: turn 1 finishes in milliseconds, leaving <1s of
        // budget. The old as_secs() check truncated (elapsed 0s < 1s) and
        // started turn 2 anyway — against a real provider that request would
        // die as a timeout Err (exit 1, no result file). The deadline floor
        // must instead end the run as stop_reason "timeout" without a second
        // chat call.
        let cfg = cfg_with(5, 1, 0);
        let (prov, state) = MockProvider::new(vec![
            mock::resp_tool("a", "echo", json!({}), None),
            mock::resp_text("done", None),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("hi")], test_ctx());
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "timeout");
        assert_eq!(o.turns, 1);
        assert_eq!(state.calls(), 1, "no request started on a sub-second budget");
    }

    #[test]
    fn timeout_one_second_still_runs_first_turn() {
        // The sub-second floor is gated on turns>0: with timeout_secs=1 the
        // FIRST turn starts with (nearly) the full budget and must run to
        // completion, not be refused up front.
        let cfg = cfg_with(5, 1, 0);
        let (prov, state) = MockProvider::new(vec![mock::resp_text("ok", None)]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("hi")], test_ctx());
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(state.calls(), 1);
    }

    #[test]
    fn usage_accumulates_across_turns_and_emits_events() {
        // Token usage from every chat sums into RunOutcome and is also emitted
        // per-turn (so the TUI bar / result file report honest totals).
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("a", "echo", json!({}), mock::mk_usage(100, 10)),
            mock::resp_text("done", mock::mk_usage(50, 5)),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("echo"))],
            vec![user_text("go")],
            test_ctx(),
        );
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.input_tokens, 150);
        assert_eq!(o.output_tokens, 15);
        let usage_n = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Usage { .. }))
            .count();
        assert_eq!(usage_n, 2);
    }

    #[test]
    fn usage_accumulation_saturates_instead_of_wrapping() {
        // B2: a lying provider reporting u64::MAX input tokens must SATURATE
        // the accumulator — a wrapping `+=` folds MAX twice into MAX-1
        // (quietly dodging any budget gate read on the total), and a debug
        // build panics outright on the second add.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("a", "echo", json!({}), mock::mk_usage(u64::MAX, 1)),
            mock::resp_text("done", mock::mk_usage(u64::MAX, 1)),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("echo"))],
            vec![user_text("go")],
            test_ctx(),
        );
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.input_tokens, u64::MAX, "two MAX folds saturate at MAX");
        assert_eq!(o.output_tokens, 2);
    }

    #[test]
    fn cache_usage_propagates_to_run_outcome_and_event() {
        // Prompt-cache tokens billed by the provider (Anthropic cache writes/
        // reads) must flow through to the RunOutcome totals AND the per-turn
        // Usage event — otherwise caching-on would under-report real cost.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool("a", "echo", json!({}), mock::mk_usage_cache(100, 10, 300, 1500)),
            mock::resp_text("done", mock::mk_usage_cache(50, 5, 0, 1200)),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("echo"))],
            vec![user_text("go")],
            test_ctx(),
        );
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        // input/output accumulate as before...
        assert_eq!(o.input_tokens, 150);
        assert_eq!(o.output_tokens, 15);
        // ...and so do the cache fields: 300+0 writes, 1500+1200 reads.
        assert_eq!(o.cache_creation_input_tokens, 300);
        assert_eq!(o.cache_read_input_tokens, 2700);
        // The per-turn Usage event carries the cache fields too.
        let cache_events = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    AgentEvent::Usage {
                        cache_creation_input_tokens: _,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(cache_events, 2);
    }

    #[test]
    fn compress_folds_history_and_emits_event() {
        // History over threshold + a safe cut → summarize runs (an extra chat
        // call), the middle folds into message[0], a Compressed event fires,
        // and the main request sees the shorter folded history.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        // 5 messages → n > keep_recent+2; cut candidate 3 = assistant → Some(3)
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", None),     // summarize_prefix call
            mock::resp_text("final answer", None), // main call
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(state.calls(), 2);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Compressed { dropped: 2, kept: 3, trigger: CompressTrigger::Preflight })),
            "compress should drop 2 middle messages, keep 3, tagged preflight"
        );
        // The main (2nd) request saw the folded history: 3 messages.
        let cap = state.captured();
        assert_eq!(cap[1].message_count, 3);
    }

    #[test]
    fn compress_with_keep_recent_zero_folds_everything() {
        // Loop-level companion to safe_cut_works_with_keep_recent_zero: with
        // keep_recent=0 the real-loop (user-final) history must still compress.
        // cut==n → the folded history is exactly the summary head, and the run
        // completes. Pre-fix this config silently never compressed (the cut
        // scan missed on user-final histories).
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 0;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"), // user-final: the real loop's request-point shape
        ];
        let (prov, state) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", None),      // summarize_prefix call
            mock::resp_text("final answer", None), // main call on folded history
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(state.calls(), 2);
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::Compressed { dropped: 4, kept: 1, trigger: CompressTrigger::Preflight }
            )),
            "empty-tail cut: drop the 4 non-head messages, keep exactly the summary head"
        );
        // The main (2nd) request saw the folded history: just the head.
        let cap = state.captured();
        assert_eq!(cap[1].message_count, 1);
    }

    #[test]
    fn overflow_recovery_with_keep_recent_zero() {
        // The overflow-escape path checks find_safe_cut(...).is_some() before
        // force-compressing; with keep_recent=0 and a user-final history that
        // check used to fail → unrecoverable overflow RunError. Now the empty
        // tail is a legal cut, the forced compression folds everything, and
        // the retried request succeeds.
        let mut cfg = cfg_with(5, 0, 0); // threshold 0: no preflight, only overflow
        cfg.limits.keep_recent_messages = 0;
        let initial = vec![
            user_text("task"),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            MockTurn::Err("mock: maximum context length exceeded".into()),
            mock::resp_text("SUMMARY", None),      // forced compression call
            mock::resp_text("final answer", None), // retried main call
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(state.calls(), 3);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Compressed { trigger: CompressTrigger::Overflow, .. })),
            "overflow should trigger a forced compression"
        );
        // The retried main request saw the folded 1-message history.
        let cap = state.captured();
        assert_eq!(cap[2].message_count, 1);
    }

    #[test]
    fn compress_preserves_tool_use_tool_result_pairing_in_tail() {
        // Integration-level companion to `safe_cut_skips_tool_pairs` (which
        // only unit-tests find_safe_cut's cut SELECTION). This runs a REAL
        // compression — summarize_prefix + fold_summary through
        // compress_if_needed — with a tool_use ↔ tool_result pair sitting at
        // the kept-tail boundary, and asserts the MAIN request still satisfies
        // the Anthropic N:N invariant over the WHOLE folded history: every
        // tool_use id has exactly one matching tool_result id (no orphans). A
        // regression that cut mid-pair, or dropped/merged a tail message, would
        // orphan one side and trip the sorted-id equality below.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40); // alone over the 30-char threshold
        // Layout (n=7, strict user/assistant alternation from [0]=user):
        //   [0] user(long)                                  ← head (summary folded in)
        //   [1] asst a1  [2] user u1  [3] asst a2  [4] user u2  ← summarized prefix
        //   [5] asst(tool_use t1, t2)  [6] user(tool_result t1, t2)  ← kept tail
        // keep_recent=2 → find_safe_cut returns cut=5 (messages[5] is the
        // assistant boundary), so the pair stays whole in the tail.
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
            Message::assistant(vec![
                Block::ToolUse {
                    id: "t1".into(),
                    name: "shell".into(),
                    input: json!({"command": "ls"}),
                },
                Block::ToolUse {
                    id: "t2".into(),
                    name: "shell".into(),
                    input: json!({"command": "pwd"}),
                },
            ]),
            Message::user(vec![
                Block::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "ls-out".into(),
                    image: None,
                    is_error: false,
                },
                Block::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "pwd-out".into(),
                    image: None,
                    is_error: false,
                },
            ]),
        ];
        let (prov, state) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", None),      // summarize_prefix call
            mock::resp_text("final answer", None), // main call on folded history
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(state.calls(), 2, "exactly summary call + main call");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Compressed { dropped: 4, kept: 3, .. })),
            "should drop 4 prefix messages, keep 3 (head + the tail pair)"
        );
        // MAIN (2nd) request: head + asst(2 tool_use) + user(2 tool_result).
        let cap = state.captured();
        let main = &cap[1];
        assert_eq!(main.message_count, 3);
        // N:N bijection over the whole folded history. Anthropic doesn't mandate
        // result order vs. use order, so compare as multisets (sorted).
        let mut uses = main.tool_use_ids.clone();
        let mut results = main.tool_result_ids.clone();
        uses.sort();
        results.sort();
        assert_eq!(
            uses, results,
            "tool_use ↔ tool_result must stay N:N after compression (no orphans)"
        );
        assert_eq!(uses.len(), 2, "both tail tool_uses survived (none dropped)");
    }

    #[test]
    fn compress_summary_request_omits_caching() {
        // The compression summary is a one-off call on a prefix that will never
        // be re-sent as a growing prefix, so it must pass cache:false (no 1.25×
        // write premium). The subsequent main turn is a normal agent turn and
        // passes cache:true. Pinned because the summary builds its own
        // ChatRequest, separate from the turn request — easy to forget.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", None),     // summarize_prefix call
            mock::resp_text("final answer", None), // main call
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        agent.run(&mut |_| {}).unwrap();
        let cap = state.captured();
        assert_eq!(cap.len(), 2);
        assert!(!cap[0].cache, "summary request must be cache:false");
        assert!(cap[1].cache, "main turn request must be cache:true");
    }

    #[test]
    fn compress_summary_usage_is_counted_in_totals() {
        // The summarize_prefix request is a real billed call — its usage must
        // fold into the run totals (and emit a Usage event), not be dropped.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, _) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", mock::mk_usage(200, 30)), // summary call
            mock::resp_text("final", mock::mk_usage(100, 10)),   // main call
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.input_tokens, 300); // 200 (summary) + 100 (main)
        assert_eq!(o.output_tokens, 40); // 30 + 10
        // The summary's usage must emit a Usage event too (not just fold into
        // the totals) — otherwise a future refactor could drop the event emit
        // while the totals still add up and this test would pass. Two calls →
        // two Usage events.
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::Usage { .. }))
                .count(),
            2,
            "summary and main chat each emit a Usage event"
        );
    }

    #[test]
    fn compress_accepts_summary_truncated_by_max_tokens() {
        // A summary that hit the output budget (stop_reason max_tokens) is
        // coherent-but-incomplete text, NOT a half-written/cancelled stream.
        // Folding the partial summary beats disabling compression: compress_failed
        // also gates overflow recovery, so disabling here would take out BOTH
        // compression paths at once. The partial summary must fold (Compressed
        // event fires, no compression-failed Warning).
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, _) = MockProvider::new(vec![
            MockTurn::Ok(ChatResponse {
                blocks: vec![Block::Text("partial summary".into())],
                stop_reason: "max_tokens".into(),
                usage: None,
            }),
            mock::resp_text("final", None),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Compressed { .. })),
            "truncated (max_tokens) summary must still fold (no Compressed event fired)"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::Warning(w) if w.contains("compression failed"))),
            "truncated summary must NOT disable compression (compress_failed was set)"
        );
    }

    #[test]
    fn compress_empty_summary_still_counts_its_usage() {
        // The model returned an empty/unusable summary, but the summarize
        // request still billed tokens. Those must be counted (honest cost) even
        // though compression is then disabled and no Compressed event fires.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, _) = MockProvider::new(vec![
            mock::resp_text("   ", mock::mk_usage(200, 30)), // empty summary, but billed
            mock::resp_text("final", mock::mk_usage(100, 10)),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        // Summary usage counted despite the empty/unusable text.
        assert_eq!(o.input_tokens, 300);
        assert_eq!(o.output_tokens, 40);
        // Both calls emitted a Usage event; compression was NOT applied.
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::Usage { .. }))
                .count(),
            2
        );
        assert!(events
            .iter()
            .all(|e| !matches!(e, AgentEvent::Compressed { .. })));
        // An empty summary silently disabling compression would leave a growing
        // context unexplained — a one-time Warning must surface the disable.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Warning(w) if w.contains("empty summary"))),
            "empty summary should warn that compression is disabled"
        );
    }

    #[test]
    fn compress_without_provider_usage_emits_no_zero_event() {
        // Compression ran (summary non-empty, history folded, Compressed fires)
        // but the provider reported NO usage for the summary request. The old
        // code synthesized a Usage{0,0} event for it (noise in NDJSON/TUI).
        // G3: no Usage event and nothing added to the totals from the summary —
        // only the main call's usage (which it did report) shows up.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, _) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", None),                  // summary ran, no usage reported
            mock::resp_text("final", mock::mk_usage(100, 10)), // main call reports usage
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        // Totals reflect ONLY the main call — the summary reported no usage, so
        // there is nothing honest to count from it.
        assert_eq!(o.input_tokens, 100);
        assert_eq!(o.output_tokens, 10);
        // Compression did run...
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compressed { dropped: 2, kept: 3, .. })));
        // ...but emitted exactly ONE Usage event (the main call's), not a
        // spurious second Usage{0,0} for the usage-less summary.
        let usage_events = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Usage { .. }))
            .collect::<Vec<_>>();
        assert_eq!(
            usage_events.len(),
            1,
            "summary with no provider usage must not emit a Usage{{0,0}} event"
        );
        assert!(matches!(
            usage_events[0],
            AgentEvent::Usage { input_tokens: 100, output_tokens: 10, .. }
        ));
    }

    #[test]
    fn compress_failure_degrades_gracefully() {
        // summarize_prefix errors → compress_failed set, NO Compressed event,
        // the run continues uncompressed and still completes.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, _) = MockProvider::new(vec![
            MockTurn::Err("summary blew up".into()), // summarize_prefix fails
            mock::resp_text("answer anyway", None),   // main call proceeds uncompressed
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::Compressed { .. })),
            "no compress event on summary failure"
        );
    }

    #[test]
    fn compress_partial_summary_is_not_folded_into_history() {
        // A summary stream cut short (cancel / idle timeout / max_tokens)
        // arrives as Ok with PARTIAL text and a non-end_turn stop_reason.
        // Folding that half-written "summary" into the history would poison
        // every later turn — it must be treated as a compression failure:
        // no Compressed event, history untouched, run continues uncompressed.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            MockTurn::Ok(ChatResponse {
                blocks: vec![Block::Text("half a summa".into())],
                stop_reason: "cancelled".into(), // partial stream, not end_turn
                usage: None,
            }),
            mock::resp_text("answer anyway", None),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial.clone(), test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::Compressed { .. })),
            "partial summary must not count as a compression"
        );
        // The main call saw the ORIGINAL 5-message history, unpolluted.
        let caps = state.captured();
        assert_eq!(caps[1].message_count, initial.len());
    }

    #[test]
    fn proactive_compress_on_token_estimate() {
        // max_context_tokens switches preflight to a token estimate (≈4
        // chars/token, reserving this turn's output). A history whose estimate
        // + output exceeds 85% of the window triggers compression BEFORE the
        // main request — same fold / event / short-history guarantees as the
        // char-threshold path, but driven by the model's real window size.
        let mut cfg = cfg_with(5, 0, 0); // char threshold off
        cfg.limits.max_context_tokens = 1000;
        cfg.limits.keep_recent_messages = 2;
        cfg.max_tokens = 100;
        let long = "x".repeat(4000); // est ≈ 1000 tokens >> 85% of 1000 (850)
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", None), // summarize_prefix call
            mock::resp_text("final", None),   // main call on folded history
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(state.calls(), 2);
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compressed { dropped: 2, kept: 3, .. })));
        let cap = state.captured();
        assert_eq!(cap[1].message_count, 3, "main call saw the folded history");
    }

    #[test]
    fn overflow_error_triggers_force_compress_then_retry() {
        // The core overflow path: the provider rejects the request as too long
        // → the agent force-compresses ONCE and retries the SAME turn on the
        // shorter history, which then succeeds. Pins OpenCode's "compact and
        // retry that step once" recovery — without it, an over-long request
        // fails the whole run instead of self-healing.
        let mut cfg = cfg_with(5, 0, 0); // no preflight; only the overflow path fires
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            MockTurn::Err(
                "OpenAI-compatible API error 400: this model's maximum context length is 8192 tokens"
                    .into(),
            ),
            mock::resp_text("SUMMARY", None),       // force-compress summary call
            mock::resp_text("recovered answer", None), // retried main call
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(o.final_text, "recovered answer");
        // overflow Err → forced summary → retried main. Exactly 3 calls.
        assert_eq!(state.calls(), 3);
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compressed { dropped: 2, kept: 3, trigger: CompressTrigger::Overflow })),
            "forced compression after the overflow error must be tagged overflow"
        );
        let cap = state.captured();
        assert_eq!(cap[0].message_count, 5, "first (overflowing) call saw full history");
        assert_eq!(cap[2].message_count, 3, "retried call saw the folded history");
    }

    #[test]
    fn overflow_second_time_propagates_as_error() {
        // After one forced compression + retry, a SECOND overflow on the same
        // turn is a real error (OpenCode: "a second overflow after recovery is
        // returned as an error"). overflow_recovered caps recovery at one per
        // turn — no infinite retry loop.
        let mut cfg = cfg_with(5, 0, 0);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, _state) = MockProvider::new(vec![
            MockTurn::Err("OpenAI-compatible API error 400: context_length_exceeded".into()),
            mock::resp_text("SUMMARY", None), // forced compress succeeds
            MockTurn::Err("Anthropic API error 400: prompt is too long: 9000 > 8192".into()),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let err = match agent.run(&mut |_| {}) {
            Err(e) => e,
            Ok(_) => panic!("second overflow must propagate as a run error"),
        };
        assert!(
            format!("{err:#}").contains("prompt is too long"),
            "second overflow surfaced, not swallowed: {err:#}"
        );
    }

    #[test]
    fn non_overflow_error_is_not_compressed() {
        // A provider error that isn't a context-overflow (rate limit, auth,
        // generic 400) must propagate IMMEDIATELY — never trigger a
        // compression. Pins that overflow detection is specific, not "any Err".
        let mut cfg = cfg_with(5, 0, 0);
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            MockTurn::Err("OpenAI-compatible API error 429: rate limit exceeded".into()),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let err = match agent.run(&mut |e| events.push(e)) {
            Err(e) => e,
            Ok(_) => panic!("non-overflow error must propagate"),
        };
        assert!(format!("{err:#}").contains("rate limit"));
        assert_eq!(state.calls(), 1, "no summary call for a non-overflow error");
        assert!(events
            .iter()
            .all(|e| !matches!(e, AgentEvent::Compressed { .. })));
    }

    #[test]
    fn overflow_with_no_compressible_head_propagates() {
        // If the history is too short to cut (find_safe_cut None) — e.g. the
        // overflow is driven by tool schemas / system prompt, not conversation —
        // there's nothing to compress: propagate the overflow as a real error
        // rather than looping or synthesizing an empty summary.
        let cfg = cfg_with(5, 0, 0); // keep_recent default 8
        let initial = vec![
            user_text("hi"),
            Message::assistant(vec![Block::Text("hello".into())]),
            user_text("more"),
        ]; // 3 messages <= keep_recent(8)+2 → find_safe_cut None
        let (prov, state) = MockProvider::new(vec![
            MockTurn::Err("Anthropic API error 400: prompt is too long".into()),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let err = match agent.run(&mut |_| {}) {
            Err(e) => e,
            Ok(_) => panic!("overflow with no compressible head must propagate"),
        };
        assert!(format!("{err:#}").contains("prompt is too long"));
        assert_eq!(state.calls(), 1, "no summary call when nothing is compressible");
    }

    #[test]
    fn is_context_overflow_distinguishes_input_from_output_token_limit() {
        // Unambiguous context-overflow phrases match (compressing may help).
        assert!(is_context_overflow(&anyhow::anyhow!("context_length_exceeded")));
        assert!(is_context_overflow(&anyhow::anyhow!("prompt is too long")));
        // A bare "token limit exceeded" is some gateways' INPUT-overflow wording
        // → matches (compressing may help).
        assert!(is_context_overflow(&anyhow::anyhow!("token limit exceeded")));
        // …but the same phrase in an OUTPUT / completion context must NOT match —
        // compressing the input won't fix a max-output-tokens error, and treating
        // it as overflow wastes one force-compress + retry that fails identically.
        assert!(!is_context_overflow(&anyhow::anyhow!("max output token limit exceeded")));
        assert!(!is_context_overflow(&anyhow::anyhow!(
            "completion token limit exceeded"
        )));
        // Control: unrelated errors never match.
        assert!(!is_context_overflow(&anyhow::anyhow!("rate limit exceeded")));
        assert!(!is_context_overflow(&anyhow::anyhow!("invalid api key")));
    }

    #[test]
    fn build_window_transcript_strips_marker_returns_old_summary() {
        // The rolling-summary mechanism: when the head already carries a summary
        // block (a later compression), build_window_transcript must (a) return
        // that prior summary as `old_summary` so the prompt asks for an UPDATE,
        // and (b) strip the marker block from the transcript so it isn't re-fed
        // as raw input. The kept tail (>= cut) must not appear in the transcript.
        let mut head = user_text("the task");
        head.content
            .push(Block::Text(format!("{SUMMARY_MARKER}\nold running summary body")));
        let initial = vec![
            head,
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        // cut=3 → summarize head + [a1, u1]; tail [a2, u2] excluded.
        let (transcript, old) = Agent::build_window_transcript(&initial, 3);
        assert_eq!(old.as_deref(), Some("old running summary body"));
        assert!(!transcript.contains(SUMMARY_MARKER), "marker block stripped");
        assert!(transcript.contains("the task"), "original head text kept");
        assert!(transcript.contains("a1") && transcript.contains("u1"));
        assert!(
            !transcript.contains("a2") && !transcript.contains("u2"),
            "kept tail must not appear in the transcript"
        );
    }

    #[test]
    fn compress_large_window_chunks_then_combines() {
        // A prefix larger than SUMMARY_CHUNK_CHARS is summarized per
        // message-aligned chunk, then the chunk summaries are combined into the
        // final brief. This bounds the per-call ratio (~6:1) so detail survives,
        // and keeps each summary call small enough that it can't itself overflow
        // — which the overflow path relies on, since it force-compresses exactly
        // when the context is huge.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let big = "x".repeat(30_000); // one block alone > SUMMARY_CHUNK_CHARS (24_000)
        let initial = vec![
            user_text(&big),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", None), // chunk 1 summary
            mock::resp_text("SUMMARY", None), // chunk 2 summary
            mock::resp_text("SUMMARY", None), // combine summary
            mock::resp_text("final", None),   // main call on folded history
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert!(!o.final_text.is_empty());
        // 2 chunk summaries + 1 combine + 1 main = 4 calls.
        assert_eq!(state.calls(), 4, "chunk path: per-chunk + combine + main");
        let cap = state.captured();
        // The combine call assembled the chunk summaries under "[segment N]".
        let combine = &cap[2];
        assert!(
            combine.user_texts.iter().any(|t| t.contains("segment")),
            "combine call must assemble chunk summaries: {:?}",
            combine.user_texts
        );
        // Every summary call opts out of caching; the main turn uses it.
        assert!(!cap[0].cache && !cap[1].cache && !cap[2].cache, "summaries cache:false");
        assert!(cap[3].cache, "main call uses prompt caching");
        // The main call saw the folded (short) history.
        assert_eq!(cap[3].message_count, 3);
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compressed { dropped: 2, kept: 3, .. })));
    }

    #[test]
    fn compress_chunk_failure_disables_for_run() {
        // If one of the chunk/combine summary calls fails, compression is
        // abandoned for this run (compress_failed), NO fold happens, and the run
        // continues uncompressed on the original history. Graceful degradation
        // — a transient summary failure must not break the whole run.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        let big = "x".repeat(30_000);
        let initial = vec![
            user_text(&big),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", None),               // chunk 1 ok
            MockTurn::Err("chunk2 summary blew up".into()), // chunk 2 fails
            mock::resp_text("answer anyway", None),         // main proceeds uncompressed
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(o.final_text, "answer anyway");
        assert!(events
            .iter()
            .all(|e| !matches!(e, AgentEvent::Compressed { .. })), "no fold on chunk failure");
        let cap = state.captured();
        assert_eq!(cap[2].message_count, 5, "no fold → main sees the full history");
    }

    #[test]
    fn compress_updates_running_summary_on_second_pass() {
        // The rolling-summary UPDATE path is this commit's core fidelity claim
        // ("later compressions update the prior summary, not re-summarize the
        // whole prefix"). build_window_transcript only unit-tests the pure
        // extraction; THIS test drives the full summarize_prefix → summary_prompt
        // wiring: with the head already carrying a SUMMARY_MARKER + old summary,
        // the summary call's prompt must carry the UPDATE marker and the prior
        // summary, and must NOT re-feed the raw marker block as transcript.
        let mut cfg = cfg_with(5, 0, 30);
        cfg.limits.keep_recent_messages = 2;
        // Head already folded by a PRIOR compression: original task + a marker
        // block holding an old running summary.
        let mut head = user_text("the original task");
        head.content.push(Block::Text(format!(
            "{SUMMARY_MARKER}\nold running summary: we built feature X"
        )));
        let long = "y".repeat(40); // trips the 30-char threshold again
        let initial = vec![
            head,
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text(&long),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            mock::resp_text("UPDATED SUMMARY", None), // summary call (update mode)
            mock::resp_text("final", None),           // main call
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compressed { .. })));
        let cap = state.captured();
        let prompt = &cap[0].user_texts[0];
        assert!(
            prompt.contains("Current running summary"),
            "update-mode prompt expected: {prompt}"
        );
        assert!(
            prompt.contains("old running summary: we built feature X"),
            "prior summary must be fed for the update: {prompt}"
        );
        assert!(
            !prompt.contains(&format!("[user] {SUMMARY_MARKER}")),
            "raw marker block must not be re-fed as transcript"
        );
    }

    #[test]
    fn compress_with_deadline_splits_timeout_without_panic() {
        // Every other compress test uses timeout_secs=0 (deadline=None), so the
        // summary call's per_request_timeout = (remaining − reserve) / n_calls
        // branch never runs. Drive it here: a real timeout must let compression
        // proceed and leave room for the main turn, without panicking on the
        // Duration division.
        let mut cfg = cfg_with(5, 60, 30); // timeout_secs=60 → deadline=Some
        cfg.limits.keep_recent_messages = 2;
        let long = "x".repeat(40);
        let initial = vec![
            user_text(&long),
            Message::assistant(vec![Block::Text("a1".into())]),
            user_text("u1"),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("u2"),
        ];
        let (prov, state) = MockProvider::new(vec![
            mock::resp_text("SUMMARY", None),
            mock::resp_text("final", None),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(state.calls(), 2, "summary + main both ran under a deadline");
    }

    #[test]
    fn token_estimate_over_but_too_short_to_cut_does_not_compress() {
        // max_context_tokens trips the token estimate, but the history is too
        // short for find_safe_cut (n <= keep_recent+2): compression is a no-op
        // (no event, no fold), and the run proceeds — the overflow path remains
        // as backstop. Pins that a too-short-to-cut history never partial-folds.
        let mut cfg = cfg_with(5, 0, 0);
        cfg.limits.max_context_tokens = 1000;
        cfg.max_tokens = 100;
        // keep_recent default 8 → 3 messages <= 8+2 → find_safe_cut None
        let big = "x".repeat(4000); // est ≈ 1000 tokens >> cap (850)
        let initial = vec![
            user_text(&big),
            Message::assistant(vec![Block::Text("a".into())]),
            user_text("b"),
        ];
        let (prov, state) = MockProvider::new(vec![mock::resp_text("ok", None)]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
        let mut events = Vec::new();
        let o = agent.run(&mut |e| events.push(e)).unwrap();
        assert_eq!(o.stop_reason, "end_turn");
        assert_eq!(state.calls(), 1, "no summary call — nothing safe to cut");
        assert!(events
            .iter()
            .all(|e| !matches!(e, AgentEvent::Compressed { .. })));
    }

    #[test]
    fn chunk_lines_splits_an_overlong_line_to_bound_chunk_size() {
        // A single transcript line larger than the chunk limit (e.g. a huge
        // read_file/shell dump, or an untruncated user paste) must be split at
        // char boundaries so NO chunk exceeds the limit — otherwise that one
        // chunk's summary call could itself overflow the window, defeating the
        // overflow-recovery path that force-compresses when context is huge.
        let one = "z".repeat(50_000);
        let chunks = chunk_lines(vec![one], 24_000);
        assert!(
            chunks.len() >= 2,
            "over-long line must be split, got {} chunks",
            chunks.len()
        );
        assert!(
            chunks.iter().all(|c| c.len() <= 24_000),
            "every chunk must be ≤ limit: sizes {:?}",
            chunks.iter().map(|c| c.len()).collect::<Vec<_>>()
        );
        // Short lines still pack into one chunk, unchanged.
        let packed = chunk_lines(vec!["a\n".into(), "b\n".into()], 24_000);
        assert_eq!(packed.len(), 1);
        assert_eq!(packed[0], "a\nb\n");
    }

    #[test]
    fn provider_error_propagates_as_run_error() {
        // A provider Err from the main chat surfaces as run()'s Result::Err —
        // the headless harness then writes a result file + exit 1.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) =
            MockProvider::new(vec![MockTurn::Err("503 service unavailable".into())]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("hi")], test_ctx());
        // Match by hand: RunOutcome isn't Debug, so unwrap_err() won't compile.
        let err = match agent.run(&mut |_| {}) {
            Err(e) => e,
            Ok(_) => panic!("expected a provider error to propagate"),
        };
        assert!(format!("{err:#}").contains("503"));
    }

    #[test]
    fn stream_error_stop_reason_maps_to_run_error_with_usage() {
        // Honest cost (CLAUDE.md "重试诚实成本"): when a provider returns
        // stop_reason "stream_error" (Anthropic mid-stream error event / read
        // failure), the agent must (1) fold the partial usage into self.usage
        // so RunError.usage bills it, and (2) return Err so harness/batch retry
        // the transient failure. Previously this path bailed inside the
        // provider, dropping usage Anthropic had already billed.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![MockTurn::Ok(ChatResponse {
            blocks: vec![
                Block::Text("partial reply".into()),
                Block::Text("[stream error] overloaded".into()),
            ],
            stop_reason: "stream_error".into(),
            usage: mock::mk_usage(1500, 7),
        })]);
        let mut agent =
            Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("hi")], test_ctx());
        let err = match agent.run(&mut |_| {}) {
            Err(e) => e,
            Ok(_) => panic!("stream_error must map to a retryable Err"),
        };
        // The error message is extracted from the [stream error] Text block.
        assert!(format!("{err:#}").contains("overloaded"), "got: {err:#}");
        // The partial usage (input 1500 + output 7) flowed into RunError.usage.
        assert_eq!(
            (err.usage.input_tokens, err.usage.output_tokens),
            (1500, 7),
            "stream_error partial usage must be billed in RunError"
        );
    }

    #[test]
    fn single_run_retries_transient_then_succeeds() {
        // harness::run_with_retries retries on agent.run() Err: two transient
        // 503s then a clean turn → success, exit 0, attempts==3 in result.json.
        // The injected sleeper is a no-op so the test doesn't wait on backoff.
        let cfg = cfg_with(5, 0, 0);
        let (prov, state) = MockProvider::new(vec![
            MockTurn::Err("503 transient".into()),
            MockTurn::Err("503 transient".into()),
            mock::resp_text("recovered", None),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("go")], test_ctx());
        let dir = std::env::temp_dir().join(format!("ta-retry-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("r.json");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), false, 2, |_| {}).unwrap();
        assert_eq!(code, 0, "recovered → exit 0");
        assert_eq!(state.calls(), 3, "two retries → three attempts");
        let s = std::fs::read_to_string(&out).unwrap();
        assert!(s.contains("\"attempts\": 3"), "attempts not recorded: {s}");
        assert!(s.contains("\"success\": true"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_run_started_at_reflects_first_attempt_not_write_time() {
        // C1: result.json's started_at must stamp the run's true beginning —
        // including the retry backoff. Pre-fix it was taken at write time, so
        // a retried run's backoff waits were erased (started_at == finished_at
        // at second precision). Real sleeps here (backoff 1s + 2s ≥ 2.25s
        // even with full negative jitter) make the second-resolution
        // timestamps differ deterministically.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _state) = MockProvider::new(vec![
            MockTurn::Err("503 transient".into()),
            MockTurn::Err("503 transient".into()),
            mock::resp_text("recovered", None),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("go")], test_ctx());
        let dir = std::env::temp_dir().join(format!("ta-retry-start-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("r.json");
        let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 2, |d| {
            std::thread::sleep(d)
        })
        .unwrap();
        assert_eq!(code, 0);
        let s = std::fs::read_to_string(&out).unwrap();
        let field = |k: &str| {
            s.split(&format!("\"{k}\": \""))
                .nth(1)
                .unwrap_or_else(|| panic!("no {k} in: {s}"))
                .split('"')
                .next()
                .unwrap()
                .to_string()
        };
        let (started, finished) = (field("started_at"), field("finished_at"));
        assert!(
            started < finished,
            "started_at must precede finished_at across retried runs: {started} vs {finished}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn single_run_write_failure_keeps_exit_code() {
        // C3: a failed result.json write must not swallow the run's exit code
        // — the old `?` degraded any non-write failure to exit 1, lying to CI
        // about what happened (e.g. a cancelled run stopped being 130). A
        // read-only output dir forces the write failure.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![MockTurn::Err("503 one".into())]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("go")], test_ctx());
        let dir = std::env::temp_dir().join(format!("ta-retry-ro-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let out = dir.join("r.json");
        let r = crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {});
        // Restore FIRST so cleanup always runs, even if the assert fails.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let code = r.expect("write failure must not turn into Err");
        assert_eq!(code, 1, "the run's own exit code is preserved");
        assert!(!out.exists(), "nothing was written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unfixable_overflow_run_error_is_deterministic() {
        // B4: an overflow the escape hatch can't fix (history uncompressible —
        // keep_recent too large for a safe cut) surfaces as a RunError whose
        // is_context_overflow() is true; harness/batch use that to skip the
        // transient retry ladder. A plain transient error is NOT overflow.
        let mut cfg = cfg_with(5, 0, 0);
        cfg.limits.keep_recent_messages = 100; // n <= keep+2 → no safe cut
        let (prov, _) = MockProvider::new(vec![MockTurn::Err(
            "mock: maximum context length is 8192 tokens, got 9000".into(),
        )]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("go")], test_ctx());
        let re = match agent.run(&mut |_| {}) {
            Err(e) => e,
            Ok(o) => panic!("expected Err, got Ok({})", o.stop_reason),
        };
        assert!(re.is_context_overflow(), "source: {re:#}");

        let (prov2, _) = MockProvider::new(vec![MockTurn::Err("503 transient".into())]);
        let mut agent2 =
            Agent::new(&cfg, Box::new(prov2), vec![], vec![user_text("go")], test_ctx());
        let re2 = match agent2.run(&mut |_| {}) {
            Err(e) => e,
            Ok(o) => panic!("expected Err, got Ok({})", o.stop_reason),
        };
        assert!(!re2.is_context_overflow());
    }

    #[test]
    fn single_run_does_not_retry_unfixable_overflow() {
        // B4 companion: the retry ladder must not fire on deterministic
        // overflow — retries=3 with a single scripted Err still makes exactly
        // ONE provider call (a retry would consume the script and surface
        // "turns exhausted" on later attempts, inflating calls()).
        let mut cfg = cfg_with(5, 0, 0);
        cfg.limits.keep_recent_messages = 100;
        let (prov, state) = MockProvider::new(vec![MockTurn::Err(
            "mock: maximum context length exceeded".into(),
        )]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("go")], test_ctx());
        let dir = std::env::temp_dir().join(format!("ta-retry-ovf-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("r.json");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), false, 3, |_| {}).unwrap();
        assert_eq!(code, 1);
        assert_eq!(
            state.calls(),
            1,
            "deterministic overflow must not enter the retry ladder"
        );
        // B4 vocabulary alignment: the single-run result.json must say
        // "context_overflow" (like batch's report), not the generic "error" —
        // CI keys off this value to tell a too-long task from a flaky provider.
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&out).expect("result.json written"),
        )
        .unwrap();
        assert_eq!(written["stop_reason"], "context_overflow");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_run_folds_failed_attempt_cost_into_final_totals() {
        // P2-3 (honest cost): a retried run's result.json must bill EVERY
        // attempt, not just the last. Attempt 1 bills 100/10 streamed tokens
        // and runs a tool (EchoTool) on turn 1, then errors on turn 2; the
        // RunError carries that partial cost. Attempt 2 succeeds billing 50/5.
        // The final outcome must fold the failed attempt's 100/10 + its tool
        // call into the totals — otherwise the real cost (tokens spent, tool
        // run, side effects on disk) is silently dropped on a retry.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _state) = MockProvider::new(vec![
            // Attempt 1, turn 1: tool_use that bills streamed usage, then…
            mock::resp_tool("t", "echo", json!({}), mock::mk_usage(100, 10)),
            // Attempt 1, turn 2: provider error → run fails carrying turn 1's cost.
            MockTurn::Err("503 transient".into()),
            // Attempt 2, turn 1: clean recovery.
            mock::resp_text("done", mock::mk_usage(50, 5)),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("echo"))],
            vec![user_text("go")],
            test_ctx(),
        );
        let dir = std::env::temp_dir().join(format!("ta-retry-fold-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("r.json");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), false, 2, |_| {}).unwrap();
        assert_eq!(code, 0, "recovered → exit 0");
        let s = std::fs::read_to_string(&out).unwrap();
        // Folded: 100 (failed attempt) + 50 (success) input; 10 + 5 output.
        assert!(
            s.contains("\"input_tokens\": 150"),
            "failed attempt's input tokens not folded in: {s}"
        );
        assert!(
            s.contains("\"output_tokens\": 15"),
            "failed attempt's output tokens not folded in: {s}"
        );
        // The failed attempt's tool call survives in the audit list.
        assert!(
            s.contains("\"name\": \"echo\""),
            "failed attempt's tool call not preserved: {s}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_run_retries_exhausted_exit_1() {
        // All attempts fail: exit 1, attempts==retries+1, result.json carries
        // the last error (stop_reason "error"). Deterministic Ok outcomes are
        // never retried — only Err is, which is exactly what the mock feeds.
        let cfg = cfg_with(5, 0, 0);
        let (prov, state) = MockProvider::new(vec![
            MockTurn::Err("503 one".into()),
            MockTurn::Err("503 two".into()),
            MockTurn::Err("503 three".into()),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("go")], test_ctx());
        let dir = std::env::temp_dir().join(format!("ta-retry2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("r.json");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), false, 2, |_| {}).unwrap();
        assert_eq!(code, 1, "exhausted → exit 1");
        assert_eq!(state.calls(), 3, "retries=2 → 3 total attempts, no more");
        let s = std::fs::read_to_string(&out).unwrap();
        assert!(s.contains("\"attempts\": 3"), "{s}");
        assert!(s.contains("\"success\": false"));
        assert!(s.contains("\"stop_reason\": \"error\""));
        assert!(s.contains("503"), "last error surfaced: {s}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_run_all_attempts_fail_still_bills_accumulated_cost() {
        // P2: when EVERY attempt fails (retries exhausted), the failure
        // result.json must still bill the tokens each attempt burned — the
        // None-outcome path folds `acc` in, it doesn't zero everything. Each
        // attempt bills on turn 1 (a tool_use) then errors on turn 2; the
        // RunError carries that turn's usage, acc accumulates across attempts,
        // and the final failure record reports the sum.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _state) = MockProvider::new(vec![
            // Attempt 1: bill 100/10 via a tool call, then error.
            mock::resp_tool("t1", "echo", json!({}), mock::mk_usage(100, 10)),
            MockTurn::Err("503 transient".into()),
            // Attempt 2: bill 50/5 via a tool call, then error.
            mock::resp_tool("t2", "echo", json!({}), mock::mk_usage(50, 5)),
            MockTurn::Err("503 transient".into()),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("echo"))],
            vec![user_text("go")],
            test_ctx(),
        );
        let dir = std::env::temp_dir().join(format!("ta-retry-fail-cost-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("r.json");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), false, 1, |_| {}).unwrap();
        assert_eq!(code, 1, "all attempts failed → exit 1");
        let s = std::fs::read_to_string(&out).unwrap();
        assert!(s.contains("\"success\": false"), "{s}");
        // Accumulated honest cost across both failed attempts: 100+50 input,
        // 10+5 output — NOT zeroed just because no attempt succeeded.
        assert!(
            s.contains("\"input_tokens\": 150"),
            "failed attempts' input tokens dropped on the failure path: {s}"
        );
        assert!(
            s.contains("\"output_tokens\": 15"),
            "failed attempts' output tokens dropped on the failure path: {s}"
        );
        // Both attempts' tool calls survive in the audit list.
        assert!(
            s.matches("\"name\": \"echo\"").count() == 2,
            "expected both attempts' tool calls in the failure record: {s}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_run_no_retry_when_retries_zero() {
        // retries=0: a single Err → exit 1, attempts==1, no retry attempted.
        let cfg = cfg_with(5, 0, 0);
        let (prov, state) = MockProvider::new(vec![MockTurn::Err("503".into())]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("go")], test_ctx());
        let dir = std::env::temp_dir().join(format!("ta-retry0-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("r.json");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
        assert_eq!(code, 1);
        assert_eq!(state.calls(), 1, "retries=0 → exactly one attempt");
        let s = std::fs::read_to_string(&out).unwrap();
        assert!(s.contains("\"attempts\": 1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_run_cancel_during_backoff_exits_130() {
        // A Ctrl-C during the retry backoff must cancel (exit 130) instead of
        // being swallowed: the next agent.run would clear the flag at entry and
        // proceed, making the first Ctrl-C look like a no-op. The sleeper
        // simulates the signal by flipping the shared cancel flag.
        let cfg = cfg_with(5, 0, 0);
        let (prov, state) = MockProvider::new(vec![
            MockTurn::Err("503".into()),
            MockTurn::Err("not reached".into()),
        ]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![user_text("go")], test_ctx());
        let cancel = Arc::new(AtomicBool::new(false));
        agent.set_cancel(cancel.clone());
        let dir = std::env::temp_dir().join(format!("ta-retry-cancel-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("r.json");
        let code = crate::harness::run_with_retries(&mut agent, Some(&out), false, 2, |_| {
            cancel.store(true, Ordering::Relaxed);
        })
        .unwrap();
        assert_eq!(code, 130, "cancel during backoff → exit 130");
        assert_eq!(state.calls(), 1, "only attempt 1 ran; cancel broke before attempt 2");
        let s = std::fs::read_to_string(&out).unwrap();
        assert!(s.contains("cancelled during retry backoff"), "{s}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── end-to-end harness verification: realistic multi-turn, tool-using
    // tasks driven through harness::run_with_retries (the full agent loop +
    // real tool execution + event flow + result.json + exit code) — the kind
    // of scenario a human writes to eval the harness, not single-event units.
    // All run in-process via MockProvider; retries=0 (retry behavior is pinned
    // by the single_run_* tests above).

    /// A tool that always returns an error result — verifies the harness
    /// records tool errors (is_error=true) while the run can still succeed.
    struct ErrTool;
    impl Tool for ErrTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: "fail".into(),
                description: "test tool that always errors".into(),
                schema: json!({"type": "object", "properties": {}}),
            }
        }
        fn run(&self, _input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
            Ok(ToolOutput::error("boom: simulated failure"))
        }
    }

    /// Unique result.json path under temp for an E2E harness test (label
    /// disambiguates parallel tests that share one pid).
    fn harness_out(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ta-e2e-{label}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        (dir.clone(), dir.join("r.json"))
    }

    #[test]
    fn harness_tool_task_records_call_tokens_and_succeeds() {
        // The canonical eval: the model calls a tool, the harness runs it for
        // real, the model answers. result.json must carry the tool call, the
        // final summary, accumulated tokens, and success — and the tool must
        // actually have executed (not merely been recorded).
        let cfg = cfg_with(5, 0, 100_000); // big compress threshold: no compression
        let (prov, state) = MockProvider::new(vec![
            mock::resp_tool("t1", "echo", json!({"x": 1}), mock::mk_usage(50, 7)),
            mock::resp_text("the answer is 42", mock::mk_usage(30, 5)),
        ]);
        let ran = Arc::new(AtomicBool::new(false));
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::tracking("echo", ran.clone()))],
            vec![user_text("go")],
            test_ctx(),
        );
        let (dir, out) = harness_out("tool");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
        assert_eq!(code, 0, "recovered → exit 0");
        assert_eq!(state.calls(), 2, "two turns");
        assert!(ran.load(Ordering::Relaxed), "the tool must actually have executed");
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(parsed["success"], true);
        // Attribution stamp: the bill names the model/provider that ran it
        // (cfg_with → model "m", provider openai) plus schema version.
        assert_eq!(parsed["model"], "m");
        assert_eq!(parsed["provider"], "openai");
        assert_eq!(parsed["schema_version"], 1);
        assert!(parsed["started_at"].as_str().unwrap().ends_with('Z'));
        assert!(parsed["summary"].as_str().unwrap().contains("42"));
        assert_eq!(parsed["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["tool_calls"][0]["name"], "echo");
        assert_eq!(parsed["tool_calls"][0]["output"].as_str().unwrap(), "ok");
        assert_eq!(parsed["tool_calls"][0]["is_error"], false);
        // Tokens accumulate across both turns (50+30 ↑, 7+5 ↓).
        assert_eq!(parsed["input_tokens"], 80);
        assert_eq!(parsed["output_tokens"], 12);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn harness_multi_turn_records_each_tool_call_in_order() {
        // A run that calls two distinct tools across two turns must record both,
        // in the order they ran.
        let cfg = cfg_with(5, 0, 100_000);
        let (prov, state) = MockProvider::new(vec![
            mock::resp_tool("t1", "a", json!({}), None),
            mock::resp_tool("t2", "b", json!({}), None),
            mock::resp_text("done", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("a")), Box::new(EchoTool::new("b"))],
            vec![user_text("go")],
            test_ctx(),
        );
        let (dir, out) = harness_out("multi");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
        assert_eq!(code, 0);
        assert_eq!(state.calls(), 3, "a, b, then done");
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        let names: Vec<&str> = parsed["tool_calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a", "b"], "tool calls recorded in order");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn harness_records_a_tool_error_while_still_succeeding() {
        // A tool that errors is recorded with is_error=true; the run can still
        // succeed when the model recovers in a following turn.
        let cfg = cfg_with(5, 0, 100_000);
        let (prov, state) = MockProvider::new(vec![
            mock::resp_tool("t1", "fail", json!({}), None),
            mock::resp_text("recovered after the error", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(ErrTool)],
            vec![user_text("go")],
            test_ctx(),
        );
        let (dir, out) = harness_out("toolerr");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
        assert_eq!(code, 0, "model recovered → exit 0");
        assert_eq!(state.calls(), 2);
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["tool_calls"][0]["is_error"], true);
        assert!(parsed["tool_calls"][0]["output"]
            .as_str()
            .unwrap()
            .contains("boom"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn harness_max_turns_runaway_exits_nonzero() {
        // A task that never stops calling tools hits the max_turns cap: exit 1,
        // stop_reason max_turns_reached, success false. More mock tool_use turns
        // than the cap guarantees the limit — not a drained script — ends the
        // run (so stop_reason is max_turns, never a mock-exhausted "error").
        let cfg = cfg_with(2, 0, 100_000);
        let (prov, _state) = MockProvider::new(vec![
            mock::resp_tool("t1", "echo", json!({}), None),
            mock::resp_tool("t2", "echo", json!({}), None),
            mock::resp_tool("t3", "echo", json!({}), None),
            mock::resp_tool("t4", "echo", json!({}), None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("echo"))],
            vec![user_text("loop forever")],
            test_ctx(),
        );
        let (dir, out) = harness_out("maxturns");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
        assert_eq!(code, 1, "max_turns → exit 1");
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(parsed["success"], false);
        assert_eq!(parsed["stop_reason"], "max_turns_reached");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn harness_json_mode_runs_clean_and_writes_result() {
        // The json=true (NDJSON) path has no end-to-end test — every other
        // harness test uses json=false. Drive a real multi-turn, tool-using run
        // through it so the per-event emit_json path (and the agent-emitted Done
        // on success) runs without panicking and result.json is still written
        // with the same contract. NDJSON line content can't be asserted from a
        // parallel test binary — its schema is locked by the event_json unit
        // tests; this guards the wiring + result file.
        let cfg = cfg_with(5, 0, 100_000);
        let (prov, _state) = MockProvider::new(vec![
            mock::resp_tool("t1", "echo", json!({"q": "hi"}), mock::mk_usage(10, 2)),
            mock::resp_text("final via ndjson", mock::mk_usage(5, 3)),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(EchoTool::new("echo"))],
            vec![user_text("go")],
            test_ctx(),
        );
        let (dir, out) = harness_out("json");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), true, 0, |_| {}).unwrap();
        assert_eq!(code, 0);
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["tool_calls"].as_array().unwrap().len(), 1);
        assert!(parsed["summary"].as_str().unwrap().contains("ndjson"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── real-build integration: the agent drives its REAL write_file + shell
    // tools to write an actual program, run it, and verify the outcome. The
    // model side is scripted (MockProvider), but the build + verification are
    // real I/O — far stronger than an EchoTool returning "ok".

    #[cfg(unix)]
    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok()
    }

    /// Real write_file + edit_file + shell tools (not test doubles), so a
    /// scripted model actually writes/edits files and runs commands. client/
    /// api_key are unused by leaf tools but required by tools::build's signature.
    #[cfg(unix)]
    fn real_leaf_tools() -> Vec<Box<dyn Tool>> {
        let cfg = cfg_with(5, 0, 100_000);
        let client = crate::provider::new_client(60);
        crate::tools::build(
            &[
                "write_file".to_string(),
                "edit_file".to_string(),
                "shell".to_string(),
            ],
            &cfg,
            &client,
            "unused-api-key",
            &[],
        )
        .expect("build real leaf tools")
    }

    /// An isolated temp workdir + ToolCtx with restrict_to_workdir on, so real
    /// write_file paths are confined (exercising the real confine() path too).
    /// The workdir is canonicalized — confine() compares against a canonicalized
    /// workdir (config load canonicalizes it), and on macOS /tmp is a symlink to
    /// /private/tmp, so a non-canonical workdir would make every starts_with
    /// check falsely reject the path as "escaping".
    #[cfg(unix)]
    fn build_ctx(label: &str) -> (ToolCtx, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ta-build-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let canon = dir.canonicalize().unwrap();
        let ctx = ToolCtx {
            workdir: canon.clone(),
            max_output_chars: 10_000,
            restrict_to_workdir: true,
            cancel: None,
            shell_timeout_secs: 120,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
        };
        (ctx, canon)
    }

    #[cfg(unix)]
    #[test]
    fn harness_builds_and_verifies_a_python_web_service() {
        if !python3_available() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        // A stdlib-only HTTP server: binds a free port (0), serves one request
        // to itself, prints the body, exits — no curl, no backgrounding, no port
        // conflicts. A single `python3 svc.py` run, fully self-contained.
        let svc = r#"import http.server, socketserver, threading, urllib.request
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"Hello from TinyAgent")
    def log_message(self, *a): pass
srv = socketserver.TCPServer(("127.0.0.1", 0), H)
port = srv.server_address[1]
threading.Thread(target=srv.serve_forever, daemon=True).start()
print(urllib.request.urlopen(f"http://127.0.0.1:{port}/").read().decode())
srv.shutdown()
"#;
        let cfg = cfg_with(5, 0, 100_000);
        let (prov, _state) = MockProvider::new(vec![
            mock::resp_tool("w", "write_file", json!({"path": "svc.py", "content": svc}), None),
            mock::resp_tool("s", "shell", json!({"command": "python3 svc.py"}), None),
            mock::resp_text("The service works: it returned Hello from TinyAgent", None),
        ]);
        let (ctx, dir) = build_ctx("websvc");
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            real_leaf_tools(),
            vec![user_text("build and verify a web service")],
            ctx,
        );
        let (rdir, out) = harness_out("websvc");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
        assert_eq!(code, 0, "build + verify run should succeed");
        // Real write_file: the file actually landed in the workdir.
        let svc_path = dir.join("svc.py");
        assert!(svc_path.exists(), "svc.py not written to workdir");
        assert!(
            std::fs::read_to_string(&svc_path)
                .unwrap()
                .contains("BaseHTTPRequestHandler"),
            "written file has the server code"
        );
        // Real shell: the service actually ran and its HTTP response was captured
        // in the shell tool's recorded output.
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(parsed["success"], true);
        let shell_out = parsed["tool_calls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"].as_str() == Some("shell"))
            .expect("shell tool call recorded")["output"]
            .as_str()
            .unwrap();
        assert!(
            shell_out.contains("Hello from TinyAgent"),
            "service response should be in shell output: {shell_out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&rdir);
    }

    #[cfg(unix)]
    #[test]
    fn harness_builds_and_runs_a_python_data_pipeline() {
        if !python3_available() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        // A stdin word-counter: write it, pipe text through it, check the count.
        let counter = "import sys\nprint(f\"words: {len(sys.stdin.read().split())}\")\n";
        let cfg = cfg_with(5, 0, 100_000);
        let (prov, _state) = MockProvider::new(vec![
            mock::resp_tool(
                "w",
                "write_file",
                json!({"path": "count.py", "content": counter}),
                None,
            ),
            mock::resp_tool(
                "s",
                "shell",
                json!({"command": "printf 'one two three four\\n' | python3 count.py"}),
                None,
            ),
            mock::resp_text("Counted 4 words", None),
        ]);
        let (ctx, dir) = build_ctx("count");
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            real_leaf_tools(),
            vec![user_text("count the words")],
            ctx,
        );
        let (rdir, out) = harness_out("count");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
        assert_eq!(code, 0);
        assert!(dir.join("count.py").exists(), "count.py not written");
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(parsed["success"], true);
        let shell_out = parsed["tool_calls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"].as_str() == Some("shell"))
            .expect("shell tool call recorded")["output"]
            .as_str()
            .unwrap();
        assert!(
            shell_out.contains("words: 4"),
            "computed word count should be in output: {shell_out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&rdir);
    }

    /// Level up from the single-file builds: the agent writes a *multi-file*
    /// project (a module + its unittest suite), runs the suite, and the real
    /// shell captures unittest's "Ran 3 tests" / "OK" summary. unittest writes
    /// its summary to STDERR, so it lands under the shell tool's `[stderr]`
    /// section — proving stderr capture works end-to-end through the harness,
    /// not just in the shell unit test. `python3 -m unittest test_calc` puts the
    /// workdir on sys.path, so `from calc import ...` in the test module
    /// resolves to the calc.py the agent just wrote.
    #[cfg(unix)]
    #[test]
    fn harness_builds_a_multi_file_project_and_runs_its_test_suite() {
        if !python3_available() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        let calc = r#"def add(a, b):
    return a + b


def factorial(n):
    r = 1
    for i in range(2, n + 1):
        r *= i
    return r


def is_prime(n):
    if n < 2:
        return False
    for i in range(2, int(n**0.5) + 1):
        if n % i == 0:
            return False
    return True
"#;
        let test_calc = r#"import unittest
from calc import add, factorial, is_prime


class TestCalc(unittest.TestCase):
    def test_add(self):
        self.assertEqual(add(2, 3), 5)

    def test_factorial(self):
        self.assertEqual(factorial(5), 120)

    def test_is_prime(self):
        self.assertTrue(is_prime(7))
        self.assertFalse(is_prime(8))


if __name__ == "__main__":
    unittest.main()
"#;
        let cfg = cfg_with(8, 0, 100_000);
        let (prov, _state) = MockProvider::new(vec![
            mock::resp_tool("w1", "write_file", json!({"path": "calc.py", "content": calc}), None),
            mock::resp_tool(
                "w2",
                "write_file",
                json!({"path": "test_calc.py", "content": test_calc}),
                None,
            ),
            mock::resp_tool(
                "s",
                "shell",
                json!({"command": "python3 -m unittest test_calc"}),
                None,
            ),
            mock::resp_text("All 3 tests passed", None),
        ]);
        let (ctx, dir) = build_ctx("multifile");
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            real_leaf_tools(),
            vec![user_text("build a calc module with tests and run them")],
            ctx,
        );
        let (rdir, out) = harness_out("multifile");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
        assert_eq!(code, 0, "multi-file build + test run should succeed");
        assert!(dir.join("calc.py").exists(), "calc.py not written");
        assert!(dir.join("test_calc.py").exists(), "test_calc.py not written");
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(parsed["success"], true);
        let shell_out = parsed["tool_calls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"].as_str() == Some("shell"))
            .expect("shell tool call recorded")["output"]
            .as_str()
            .unwrap();
        // unittest prints its summary to stderr → captured under [stderr].
        assert!(
            shell_out.contains("Ran 3 tests"),
            "unittest 'Ran 3 tests' summary (on stderr) should be captured: {shell_out}"
        );
        assert!(
            shell_out.contains("OK"),
            "unittest 'OK' result line should be captured: {shell_out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&rdir);
    }

    /// The iterative debug loop: write a *buggy* program, run it and observe
    /// the wrong output, edit_file the one-line fix, re-run and observe the
    /// right output. This is the first test to exercise the real edit_file
    /// tool end-to-end through the harness (not just its unit tests) AND to
    /// assert tool_calls are recorded in execution order across turns — the
    /// first shell prints 24 (factorial(5) with an off-by-one `range(2, n)`),
    /// the second prints 120 (after `range(2, n + 1)`). It also guards a real
    /// footgun: real_leaf_tools() must register edit_file, or the tool_result
    /// silently becomes "unknown tool: edit_file" and the run still "succeeds"
    /// without ever fixing the bug.
    #[cfg(unix)]
    #[test]
    fn harness_edits_a_bug_and_re_verifies_the_fix() {
        if !python3_available() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        // Buggy factorial: range(2, n) stops one short, so 5! = 2*3*4 = 24.
        let buggy = r#"import sys


def factorial(n):
    r = 1
    for i in range(2, n):
        r *= i
    return r


print(factorial(int(sys.argv[1])))
"#;
        let cfg = cfg_with(8, 0, 100_000);
        let (prov, _state) = MockProvider::new(vec![
            mock::resp_tool("w", "write_file", json!({"path": "prog.py", "content": buggy}), None),
            mock::resp_tool(
                "s1",
                "shell",
                json!({"command": "python3 prog.py 5"}),
                None,
            ),
            mock::resp_tool(
                "e",
                "edit_file",
                json!({"path": "prog.py", "old_string": "range(2, n)", "new_string": "range(2, n + 1)"}),
                None,
            ),
            mock::resp_tool(
                "s2",
                "shell",
                json!({"command": "python3 prog.py 5"}),
                None,
            ),
            mock::resp_text("Fixed: 5! = 120", None),
        ]);
        let (ctx, dir) = build_ctx("editbug");
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            real_leaf_tools(),
            vec![user_text("find the factorial bug, fix it, re-verify")],
            ctx,
        );
        let (rdir, out) = harness_out("editbug");
        let code =
            crate::harness::run_with_retries(&mut agent, Some(&out), false, 0, |_| {}).unwrap();
        assert_eq!(code, 0, "edit-and-reverify run should succeed");
        // The real edit_file mutated the file on disk: the fix is present, the
        // buggy line is gone.
        let patched = std::fs::read_to_string(dir.join("prog.py")).unwrap();
        assert!(
            patched.contains("range(2, n + 1)"),
            "patched file should contain the fix: {patched}"
        );
        assert!(
            !patched.contains("for i in range(2, n):"),
            "buggy line should be gone after edit: {patched}"
        );
        // tool_calls are in execution order: first shell = buggy 24, second =
        // fixed 120. (filter, not find: there are two shell calls this time.)
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(parsed["success"], true);
        let shell_runs: Vec<&str> = parsed["tool_calls"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["name"].as_str() == Some("shell"))
            .map(|c| c["output"].as_str().unwrap())
            .collect();
        assert_eq!(
            shell_runs.len(),
            2,
            "expected two shell runs (buggy then fixed)"
        );
        assert!(
            shell_runs[0].contains("24"),
            "first (buggy) run should print 24: {}",
            shell_runs[0]
        );
        assert!(
            shell_runs[1].contains("120"),
            "second (fixed) run should print 120: {}",
            shell_runs[1]
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&rdir);
    }

    #[test]
    fn normalize_adjacent_merges_same_role_neighbors() {
        // [user a][user b] → one user with both blocks; alternation preserved.
        let mut msgs = vec![user_text("a"), user_text("b")];
        normalize_adjacent(&mut msgs);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[0].content.len(), 2);

        // A run of three same-role messages folds into one.
        let mut msgs = vec![user_text("a"), user_text("b"), user_text("c")];
        normalize_adjacent(&mut msgs);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content.len(), 3);

        // Already-alternating history is untouched.
        let mut msgs = vec![user_text("a"), Message::assistant(vec![Block::Text("x".into())])];
        normalize_adjacent(&mut msgs);
        assert_eq!(msgs.len(), 2);

        // Mixed: [u][u][a][a][u] → [u][a][u], block order preserved.
        let mut msgs = vec![
            user_text("1"),
            user_text("2"),
            Message::assistant(vec![Block::Text("a1".into())]),
            Message::assistant(vec![Block::Text("a2".into())]),
            user_text("3"),
        ];
        normalize_adjacent(&mut msgs);
        assert_eq!(msgs.len(), 3);
        assert_eq!(
            msgs.iter().map(|m| m.role).collect::<Vec<_>>(),
            vec![Role::User, Role::Assistant, Role::User]
        );
        assert_eq!(msgs[0].content.len(), 2); // "1"+"2"
        assert_eq!(msgs[1].content.len(), 2); // "a1"+"a2"
        assert_eq!(msgs[2].content.len(), 1); // "3"
    }

    #[test]
    fn normalize_adjacent_preserves_tool_use_tool_result_pairing() {
        // Folding a user message carrying a tool_result into an adjacent user
        // message must keep the tool_result AFTER its assistant tool_use — they
        // live in different roles and never merge across, so N:N pairing and id
        // survival hold. Locks the claim that the safety net can't corrupt a
        // tool history.
        let tu = Block::ToolUse {
            id: "x".into(),
            name: "sh".into(),
            input: json!({}),
        };
        let tr = Block::ToolResult {
            tool_use_id: "x".into(),
            content: "ok".into(),
            image: None,
            is_error: false,
        };
        let mut msgs = vec![
            Message::assistant(vec![tu]),
            Message::user(vec![tr]),
            Message::user(vec![Block::Text("more".into())]),
        ];
        normalize_adjacent(&mut msgs);
        assert_eq!(msgs.len(), 2, "two adjacent user messages fold into one");
        assert_eq!(msgs[0].role, Role::Assistant);
        assert_eq!(msgs[1].role, Role::User);
        // tool_use still leads, in the assistant message; tool_result + text
        // merged into the trailing user message, order preserved.
        assert!(matches!(
            &msgs[0].content[0],
            Block::ToolUse { id, .. } if id == "x"
        ));
        assert_eq!(msgs[1].content.len(), 2);
        assert!(matches!(
            &msgs[1].content[0],
            Block::ToolResult { tool_use_id, .. } if tool_use_id == "x"
        ));
        assert!(matches!(&msgs[1].content[1], Block::Text(t) if t == "more"));
    }

    #[test]
    fn run_normalizes_adjacent_user_messages_before_request() {
        // A resumed/hand-edited history with two trailing user messages would
        // make Anthropic 400. The loop must fold them into one before the
        // first chat — assert the provider sees strictly alternating roles.
        let cfg = cfg_with(5, 0, 0);
        let (prov, state) = MockProvider::new(vec![mock::resp_text("done", None)]);
        // Two adjacent user messages at the head.
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![],
            vec![user_text("first"), user_text("second")],
            test_ctx(),
        );
        let o = agent.run(&mut |_| {}).unwrap();
        assert_eq!(o.stop_reason, "end_turn");

        let cap = state.captured();
        assert_eq!(cap.len(), 1, "exactly one chat call");
        let roles = &cap[0].roles;
        assert_eq!(
            roles,
            &vec![Role::User],
            "two adjacent user messages were folded into one before the request"
        );
        assert_eq!(cap[0].message_count, 1);
        // No two adjacent roles equal (trivially true here, but pins the rule).
        for w in roles.windows(2) {
            assert_ne!(w[0], w[1], "adjacent same-role leaked: {roles:?}");
        }
    }

    // ── `todo` plan derivation (feature A) ────────────────────────────────
    // `extract_plan` derives the final plan from the records log (the last
    // non-error `todo` call's input). Records live outside `messages`, so the
    // derivation is immune to context compression — that's the whole reason it
    // lives here rather than as an Agent field.

    fn todo_rec(todos: Value, is_error: bool) -> ToolCallRecord {
        ToolCallRecord {
            name: "todo".into(),
            input: todos,
            output: "rendered".into(),
            is_error,
            meta: None,
        }
    }

    #[test]
    fn extract_plan_picks_last_nonerror_todo() {
        let recs = vec![
            todo_rec(json!({"todos":[{"subject":"a","status":"pending"}]}), false),
            ToolCallRecord {
                name: "shell".into(),
                input: json!({}),
                output: "y".into(),
                is_error: false,
                meta: None,
            },
            todo_rec(
                json!({"todos":[
                    {"subject":"a","status":"completed"},
                    {"subject":"b","status":"in_progress"}
                ]}),
                false,
            ),
        ];
        let plan = extract_plan(&recs).expect("last todo wins");
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].status, "completed");
        assert_eq!(plan[1].subject, "b");
    }

    #[test]
    fn extract_plan_skips_errored_todo_and_falls_back() {
        // A `todo` call that errored carries no usable plan (its input may be
        // the malformed thing the tool rejected) — fall back to the last good one.
        let recs = vec![
            todo_rec(json!({"todos":[{"subject":"a","status":"pending"}]}), false),
            todo_rec(json!({"todos":[{"subject":"bad"}]}), true),
        ];
        let plan = extract_plan(&recs).expect("errored todo skipped");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].subject, "a");
    }

    #[test]
    fn extract_plan_none_when_no_todo() {
        let recs = vec![ToolCallRecord {
            name: "shell".into(),
            input: json!({}),
            output: "y".into(),
            is_error: false,
            meta: None,
        }];
        assert!(extract_plan(&recs).is_none());
    }

    #[test]
    fn extract_plan_none_on_malformed_input() {
        // Defensive parse: garbage in the records (shouldn't happen — the tool
        // validates — but a hand-built record could) must never poison result.json.
        assert!(extract_plan(&[todo_rec(json!({"todos":"notarray"}), false)]).is_none());
        assert!(extract_plan(&[todo_rec(json!({}), false)]).is_none());
        assert!(
            extract_plan(&[todo_rec(
                json!({"todos":[{"subject":"a","status":"bogus"}]}),
                false
            )])
            .is_none(),
            "bad status → None"
        );
    }

    #[test]
    fn todo_loop_final_plan_reflects_last_call() {
        // Drive the real agent loop with the real `todo` tool: two `todo` calls
        // then end_turn. The derived plan must be the LAST call's state (full
        // replacement), proving the records-log derivation works end-to-end.
        // Because records live outside `messages`, this same derivation is what
        // keeps the plan correct even after context compression folds the old
        // `todo` tool_results into the summary.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool(
                "t1",
                "todo",
                json!({"todos":[
                    {"subject":"step a","status":"pending"},
                    {"subject":"step b","status":"pending"}
                ]}),
                None,
            ),
            mock::resp_tool(
                "t2",
                "todo",
                json!({"todos":[
                    {"subject":"step a","status":"completed"},
                    {"subject":"step b","status":"in_progress"}
                ]}),
                None,
            ),
            mock::resp_text("all done", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(crate::tools::todo::Todo)],
            vec![user_text("do the thing")],
            test_ctx(),
        );
        let o = agent.run(&mut |_| {}).unwrap();
        let plan = extract_plan(&o.tool_calls).expect("plan derived from records");
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].subject, "step a");
        assert_eq!(plan[0].status, "completed", "last call's state wins");
        assert_eq!(plan[1].status, "in_progress");
        // Both todo calls are in the records log.
        let todo_calls = o.tool_calls.iter().filter(|r| r.name == "todo").count();
        assert_eq!(todo_calls, 2);
    }

    #[test]
    fn todo_loop_emits_todoupdate_event() {
        // Each `todo` call streams an AgentEvent::TodoUpdate so --json pipelines
        // can render live progress.
        let cfg = cfg_with(5, 0, 0);
        let (prov, _) = MockProvider::new(vec![
            mock::resp_tool(
                "t1",
                "todo",
                json!({"todos":[{"subject":"only","status":"in_progress"}]}),
                None,
            ),
            mock::resp_text("done", None),
        ]);
        let mut agent = Agent::new(
            &cfg,
            Box::new(prov),
            vec![Box::new(crate::tools::todo::Todo)],
            vec![user_text("go")],
            test_ctx(),
        );
        let mut events = Vec::new();
        agent.run(&mut |e| events.push(e)).unwrap();
        let updates: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TodoUpdate { todos } => Some(todos.len()),
                _ => None,
            })
            .collect();
        assert_eq!(updates, vec![1], "exactly one TodoUpdate with the 1-item plan");
    }

    #[test]
    fn system_with_env_prepends_block_and_keeps_base() {
        // base present → env block first, blank-line separator, then the caller
        // prompt in full.
        let s = system_with_env(Some("You are a coder."), "macos", "sh -c", "/tmp/proj")
            .expect("Some");
        assert!(s.starts_with("# Environment\n"));
        assert!(s.contains("- OS: macos"));
        assert!(s.contains("- Shell: sh -c"));
        assert!(s.contains("- Working directory: /tmp/proj"));
        assert!(s.contains("\n\nYou are a coder."));
        // base absent → env block stands alone (still Some, no dangling blank).
        let alone = system_with_env(None, "linux", "sh -c", "/home").expect("Some");
        assert!(alone.starts_with("# Environment\n"));
        assert!(!alone.contains("\n\n"));
        // empty base behaves like None.
        let empty = system_with_env(Some(""), "linux", "sh -c", "/x").expect("Some");
        assert!(!empty.contains("\n\n"));
    }

    #[test]
    fn system_with_env_windows_vs_unix_shell_label() {
        // Cross-platform shape is verifiable off-platform: the shell fact tracks
        // the exact invocation shell.rs uses (sh -c on unix, cmd /C on windows).
        let win = system_with_env(None, "windows", "cmd /C", r"C:\proj").unwrap();
        assert!(win.contains("- OS: windows"));
        assert!(win.contains("- Shell: cmd /C"));
        assert!(win.contains(r"- Working directory: C:\proj"));

        let lin = system_with_env(None, "linux", "sh -c", "/home/x").unwrap();
        assert!(lin.contains("- Shell: sh -c"));
        assert!(!lin.contains("cmd /C"));
    }

    #[test]
    fn agent_constructs_and_runs_with_environment_injection_on() {
        // inject_environment defaults false in cfg_with for hermeticism; flip it
        // on explicitly and confirm Agent::new (reading ctx.workdir, building the
        // env block) doesn't panic and a trivial loop still completes. The mock
        // can't observe the system prompt, so this is a construction + smoke check.
        let mut cfg = cfg_with(3, 30, 0);
        cfg.inject_environment = true;
        cfg.system_prompt = Some("base prompt".into());
        let (prov, _) = MockProvider::new(vec![mock::resp_text("ok", None)]);
        let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![], test_ctx());
        let outcome = agent.run(&mut |_| {});
        let o = outcome.expect("run completes");
        assert_eq!(o.stop_reason, "end_turn");
    }
