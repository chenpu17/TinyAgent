    use super::*;
    use crate::compress::*;
    use crate::provider::mock::{self, MockProvider, MockTurn};
    use crate::provider::ChatResponse;
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
            r#"{"provider":"openai","api_key_env":"MOCK_API_KEY","model":"m"}"#,
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

    /// A stand-in for the real `task` sub-agent tool, named "task" so the loop's
    /// partition routes it to the PARALLEL batch (the whole point of commit 2).
    /// It reads `input["label"]` to make each call's output distinguishable, emits
    /// a name-tagged SubAgent event (so we can assert it ran), and bills usage
    /// (so we can assert parallel cost folds into the run totals). Concurrency is
    /// structural — we assert functional correctness, NOT timing (which would flake).
    struct TaskTool;
    impl Tool for TaskTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: "task".into(),
                description: "test stand-in for the task sub-agent tool".into(),
                schema: json!({"type": "object", "properties": {"label": {"type": "string"}}}),
            }
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
            })
        }
    }

    /// Like `TaskTool` but each call sleeps, so a timing assertion can tell real
    /// parallelism (thread::scope fan-out) from a serialization regression: two
    /// calls overlap → ~1× the sleep; serialized → ~2×. Named "task" so the loop
    /// routes it to the parallel batch. Used by the ONE timing-aware test in the
    /// suite — every other concurrency test asserts function only (see TaskTool).
    struct SlowTaskTool;
    impl Tool for SlowTaskTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: "task".into(),
                description: "test stand-in that sleeps, to assert real parallelism".into(),
                schema: json!({"type": "object", "properties": {"label": {"type": "string"}}}),
            }
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
            std::thread::sleep(std::time::Duration::from_millis(500));
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
        // The ONLY timing assertion in the suite. Two `task` calls each sleep
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
            vec![Box::new(SlowTaskTool)],
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
                .any(|e| matches!(e, AgentEvent::Compressed { dropped: 2, kept: 3 })),
            "compress should drop 2 middle messages, keep 3"
        );
        // The main (2nd) request saw the folded history: 3 messages.
        let cap = state.captured();
        assert_eq!(cap[1].message_count, 3);
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
                .any(|e| matches!(e, AgentEvent::Compressed { dropped: 4, kept: 3 })),
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
            .any(|e| matches!(e, AgentEvent::Compressed { dropped: 2, kept: 3 })));
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
            .any(|e| matches!(e, AgentEvent::Compressed { dropped: 2, kept: 3 })));
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
            .any(|e| matches!(e, AgentEvent::Compressed { dropped: 2, kept: 3 })));
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
            .any(|e| matches!(e, AgentEvent::Compressed { dropped: 2, kept: 3 })));
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
