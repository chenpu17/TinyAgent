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

// ─── Tool-result pruning (the pre-LLM context gate) ────────────────

/// [assistant tool_use][user tool_result of `size` chars] for tool `name`.
fn big_tool_turn(id: &str, name: &str, size: usize) -> Vec<Message> {
    vec![
        Message::assistant(vec![Block::ToolUse {
            id: id.into(),
            name: name.into(),
            input: json!({}),
        }]),
        Message::user(vec![Block::ToolResult {
            tool_use_id: id.into(),
            content: "x".repeat(size),
            image: None,
            is_error: false,
        }]),
    ]
}

#[test]
fn prune_replaces_old_large_results_and_keeps_pairs() {
    // [task][big pair A][big pair B][…]: protect_recent=2 → only A's result
    // (index 2) is prunable; B's (index 4) sits inside the protection window.
    let mut msgs = vec![user_text("task")];
    msgs.extend(big_tool_turn("ta", "shell", 25_000));
    msgs.extend(big_tool_turn("tb", "shell", 25_000));
    let (out, touched, freed) = prune_tool_results(&msgs, 2).expect("25k freed clears the minimum");
    assert_eq!(
        out.len(),
        msgs.len(),
        "prune replaces blocks in place, never removes messages"
    );
    assert_eq!(
        touched,
        vec![2],
        "only the tool_result message (index 2) lost content"
    );
    assert!(freed >= PRUNE_MINIMUM_CHARS);
    // Pairing intact: same tool_use_id, placeholder content, flags kept.
    let Block::ToolResult {
        tool_use_id,
        content,
        is_error,
        image,
    } = &out[2].content[0]
    else {
        panic!("result block survived");
    };
    assert_eq!(tool_use_id, "ta");
    assert_eq!(content, "[pruned tool output, was 25000 chars]");
    assert!(!*is_error);
    assert!(image.is_none());
    // The protected window passes through untouched.
    let Block::ToolResult { content, .. } = &out[4].content[0] else {
        panic!("protected result");
    };
    assert_eq!(content.len(), 25_000);
    // And the tool_use side is verbatim — the model still knows the call
    // happened and can re-run it for the details.
    assert!(
        matches!(&out[1].content[0], Block::ToolUse { id, .. } if id == "ta"),
        "tool_use block untouched"
    );
}

#[test]
fn prune_exempts_todo_results() {
    // The rendered todo checklist is the model's only live plan view — even
    // an oversized one must survive (nothing else prunable → no rewrite).
    let mut msgs = vec![user_text("task")];
    msgs.extend(big_tool_turn("t1", "todo", 30_000));
    msgs.push(user_text("tail1"));
    msgs.push(Message::assistant(vec![Block::Text("done".into())]));
    assert!(
        prune_tool_results(&msgs, 2).is_none(),
        "todo-only bulk: nothing prunable"
    );
}

#[test]
fn prune_requires_minimum_freed_chars() {
    // A single 5k result is eligible but under the 20k minimum: rewriting
    // history (and breaking the prompt-cache prefix) for peanuts is a loss.
    let mut msgs = vec![user_text("task")];
    msgs.extend(big_tool_turn("t1", "shell", 5_000));
    msgs.push(user_text("tail1"));
    msgs.push(Message::assistant(vec![Block::Text("done".into())]));
    assert!(prune_tool_results(&msgs, 2).is_none());
}

#[test]
fn prune_short_history_is_all_protected() {
    let mut msgs = vec![user_text("task")];
    msgs.extend(big_tool_turn("t1", "shell", 30_000));
    assert!(
        prune_tool_results(&msgs, 8).is_none(),
        "n <= protect_recent: the window covers everything"
    );
}

#[test]
fn prune_drops_image_payloads() {
    // Images are the most expensive per-token content; the payload drops
    // with the text and counts toward the freed budget — on the SAME /100
    // yardstick `total_chars` uses for images (the raw-length count used to
    // overstate an image's relief ~100×, letting prune "succeed" without
    // actually fitting the window back under the threshold).
    let mut msgs = vec![user_text("task")];
    msgs.push(Message::assistant(vec![Block::ToolUse {
        id: "t1".into(),
        name: "load_image".into(),
        input: json!({}),
    }]));
    msgs.push(Message::user(vec![Block::ToolResult {
        tool_use_id: "t1".into(),
        content: "x".repeat(25_000), // dominates the freed budget (> minimum)
        image: Some(Box::new(Block::Image {
            media_type: "image/png".into(),
            data: "a".repeat(40_000), // counts as 400 freed (len / 100)
        })),
        is_error: false,
    }]));
    msgs.push(user_text("tail"));
    msgs.push(Message::assistant(vec![Block::Text("done".into())]));
    let (out, _, freed) = prune_tool_results(&msgs, 2).expect("image payload counts as freed");
    assert!(freed >= PRUNE_MINIMUM_CHARS);
    // /100 yardstick pinned: the old raw-len accounting would report ~40k
    // MORE freed than the window actually shrinks (≈65k vs ≈25.4k here).
    assert!(freed < 26_000, "image freed must be len/100: {freed}");
    let Block::ToolResult { image, .. } = &out[2].content[0] else {
        panic!("result block");
    };
    assert!(image.is_none(), "image payload drops with the text");
}

#[test]
fn fold_summary_empty_tail_is_legal() {
    // cut == messages.len() (keep_recent=0): the compressed history is
    // exactly one user message — the retained head carrying the summary
    // plus verbatim quotes of the middle user texts.
    let msgs = vec![
        user_text("task"),
        Message::assistant(vec![Block::Text("r1".into())]),
        user_text("more"),
        user_text("again"),
    ];
    let out = fold_summary(&msgs, msgs.len(), "the summary");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].role, Role::User);
    // task + quote(more) + quote(again) + summary
    assert_eq!(out[0].content.len(), 4);
    assert!(matches!(&out[0].content[1], Block::Text(t)
        if t.starts_with(USER_QUOTE_MARKER) && t.contains("more")));
    assert!(matches!(&out[0].content[2], Block::Text(t)
        if t.starts_with(USER_QUOTE_MARKER) && t.contains("again")));
    assert!(matches!(&out[0].content[3], Block::Text(t) if t.contains("the summary")));
}

#[test]
fn fold_summary_empty_messages_get_fresh_head_not_panic() {
    // `first()` fallback (P3): fold_summary is pub(crate) — a caller handing
    // in an empty slice must get the fresh empty-user-head branch, not an
    // index panic on messages[0].
    let out = fold_summary(&[], 0, "s");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].role, Role::User);
    assert!(matches!(&out[0].content[0], Block::Text(t) if t.contains("s")));
}

#[test]
fn fold_summary_cut_beyond_len_keeps_head_only_not_panic() {
    // Pathological-cut twin of the empty-slice test above (external
    // re-review P2): the quote scan and the tail-peek already used
    // get(..) defensive slices, but the final extend still indexed
    // `&messages[cut..]` — cut > len panicked at the very end of the fold,
    // AFTER all the expensive work. find_safe_cut never produces such a
    // cut, yet fold_summary is pub(crate): a bad caller must degrade to
    // head-only, matching what `with_capacity`'s saturating_sub already
    // promised for the count.
    let msgs = vec![
        user_text("task"),
        Message::assistant(vec![Block::Text("a1".into())]),
        user_text("u1"),
    ];
    let out = fold_summary(&msgs, msgs.len() + 7, "s");
    assert_eq!(
        out.len(),
        1,
        "degenerate cut folds everything into the head"
    );
    assert!(matches!(&out[0].content.last(), Some(Block::Text(t)) if t.contains("s")));
}

#[test]
fn window_lines_cut_beyond_len_yields_empty_not_panic() {
    // Same defensive-slice discipline for the transcript formatter: the old
    // `messages[..cut]` panicked on cut > len. An empty transcript on a
    // degenerate cut degrades the summary prompt; a panic kills the run.
    let msgs = vec![user_text("task"), user_text("u1")];
    assert!(window_lines(&msgs, msgs.len() + 3).is_empty());
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
    // task + quote(old turn) + summary-one
    assert_eq!(msgs[0].content.len(), 3);

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
    // The fresh head carries the verbatim quote of the folded-away user
    // turn plus the summary — and none of the stray opening text.
    assert_eq!(out[0].content.len(), 2);
    assert!(matches!(&out[0].content[0], Block::Text(t)
        if t.starts_with(USER_QUOTE_MARKER) && t.contains("real turn")));
    assert!(matches!(&out[0].content[1], Block::Text(t) if t.contains("the summary")));
    // Tail preserved as-is.
    assert_eq!(out[1].role, Role::Assistant);
}

/// [assistant todo tool_use][user rendered-checklist tool_result] pair.
fn todo_turn(id: &str, rendered: &str) -> Vec<Message> {
    vec![
        Message::assistant(vec![Block::ToolUse {
            id: id.into(),
            name: "todo".into(),
            input: json!({"todos": []}),
        }]),
        Message::user(vec![Block::ToolResult {
            tool_use_id: id.into(),
            content: rendered.into(),
            image: None,
            is_error: false,
        }]),
    ]
}

#[test]
fn fold_summary_restores_todo_checklist_when_folded_away() {
    // The fold removed the model's last todo result (its only plan view —
    // the tool has no read mode): the latest checklist must come back as a
    // TODO_MARKER block in the head.
    let mut msgs = vec![user_text("task")];
    msgs.extend(todo_turn("t1", "[x] step one\n[ ] step two"));
    msgs.push(user_text("u1"));
    msgs.push(Message::assistant(vec![Block::Text("a2".into())]));
    msgs.push(user_text("u2"));
    // cut=3 folds the todo pair away; tail = [u1, a2, u2].
    let out = fold_summary(&msgs, 3, "s");
    let todo_blocks: Vec<_> = out[0]
        .content
        .iter()
        .filter(|b| matches!(b, Block::Text(t) if t.starts_with(TODO_MARKER)))
        .collect();
    assert_eq!(todo_blocks.len(), 1, "exactly one restored checklist");
    assert!(
        matches!(&todo_blocks[0], Block::Text(t) if t.contains("step two")),
        "the checklist text rides in the restored block"
    );
    // Ordering: quotes (none here) → todo → summary last.
    assert!(matches!(out[0].content.last(), Some(Block::Text(t)) if t.starts_with(SUMMARY_MARKER)));
}

#[test]
fn fold_summary_skips_todo_restore_when_tail_carries_it() {
    // The kept tail still has the live todo result — re-injecting a copy
    // would only duplicate it.
    let mut msgs = vec![user_text("task")];
    msgs.push(user_text("u1"));
    msgs.extend(todo_turn("t1", "[ ] only step"));
    msgs.push(Message::assistant(vec![Block::Text("a2".into())]));
    // cut=2: the todo pair lives in the tail.
    let out = fold_summary(&msgs, 2, "s");
    assert!(
        !out[0]
            .content
            .iter()
            .any(|b| matches!(b, Block::Text(t) if t.starts_with(TODO_MARKER))),
        "no restore when the tail carries the live checklist"
    );
}

#[test]
fn latest_todo_checklist_prefers_latest_call() {
    // Full-replacement semantics: the LAST todo call is authoritative, so
    // its rendered result is what a fold restores.
    let mut msgs = vec![user_text("task")];
    msgs.extend(todo_turn("t1", "OLD LIST"));
    msgs.extend(todo_turn("t2", "NEW LIST"));
    assert_eq!(latest_todo_checklist(&msgs).as_deref(), Some("NEW LIST"));
    // No todo call at all → nothing to restore.
    assert_eq!(latest_todo_checklist(&[user_text("task")]), None);
}

#[test]
fn fold_summary_caps_user_quotes_fifo() {
    // A single oversized middle user text is truncated at the per-message
    // cap; many quotes share the total budget with the OLDEST dropped first.
    let mut msgs = vec![user_text("task")];
    for i in 0..10 {
        msgs.push(Message::assistant(vec![Block::Text(format!("a{i}"))]));
        msgs.push(user_text(&"user text ".repeat(150))); // ~1.65k chars each
    }
    let out = fold_summary(&msgs, msgs.len(), "s");
    let quotes: Vec<&String> = out[0]
        .content
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) if t.starts_with(USER_QUOTE_MARKER) => Some(t),
            _ => None,
        })
        .collect();
    let total: usize = quotes.iter().map(|q| q.len()).sum();
    assert!(
        total <= USER_QUOTE_TOTAL_CHARS,
        "quote budget respected (total {total})"
    );
    assert!(
        quotes.len() < 10,
        "oldest quotes dropped FIFO to fit the budget"
    );

    // Per-message cap: one giant paste is truncated with the standard note.
    let msgs = vec![user_text("task"), user_text(&"x".repeat(5_000))];
    let out = fold_summary(&msgs, msgs.len(), "s");
    assert!(matches!(&out[0].content[1], Block::Text(t)
        if t.starts_with(USER_QUOTE_MARKER) && t.contains("[truncated, limit")));
}

#[test]
fn fold_summary_second_fold_keeps_old_era_quotes() {
    // Two folds, two eras: quotes preserved by fold #1 must survive fold #2
    // ("Survives later folds" is the whole point of re-collecting them from
    // the head). The collection used to scan the already-stripped head clone,
    // where it could never match — era-1 quotes silently vanished on the
    // second fold.
    let mut msgs = vec![user_text("task")];
    msgs.push(user_text("first era question"));
    msgs.push(Message::assistant(vec![Block::Text("a1".into())]));
    msgs.push(user_text("another first era question"));
    // Fold #1 swallows everything (empty tail): head + quotes only.
    let out1 = fold_summary(&msgs, msgs.len(), "era-1 summary");
    assert_eq!(
        out1[0]
            .content
            .iter()
            .filter(|b| matches!(b, Block::Text(t) if t.starts_with(USER_QUOTE_MARKER)))
            .count(),
        2,
        "first fold keeps both middle user texts verbatim"
    );
    // Era 2 stacks on the folded head, then folds again.
    let mut msgs2 = out1;
    msgs2.push(user_text("second era question"));
    let out2 = fold_summary(&msgs2, msgs2.len(), "era-2 summary");
    let quotes: Vec<&String> = out2[0]
        .content
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) if t.starts_with(USER_QUOTE_MARKER) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(
        quotes.len(),
        3,
        "era-1 quotes survive fold #2, the era-2 quote joins them"
    );
    // Era order: oldest first — the FIFO budget evicts across eras correctly.
    assert!(quotes[0].contains("first era question"));
    assert!(quotes[1].contains("another first era question"));
    assert!(quotes[2].contains("second era question"));
    // The new fold replaces, never stacks: exactly one summary, last block.
    assert_eq!(
        out2[0]
            .content
            .iter()
            .filter(|b| matches!(b, Block::Text(t) if t.starts_with(SUMMARY_MARKER)))
            .count(),
        1
    );
    assert!(
        matches!(out2[0].content.last(), Some(Block::Text(t)) if t.starts_with(SUMMARY_MARKER)),
        "summary stays the closing head block"
    );
}

#[test]
fn window_lines_skips_all_head_marker_blocks() {
    // Head marker blocks (summary/todo/quotes) are structural, not raw
    // conversation — the summarizer's transcript must not re-feed them
    // (quotes especially: paraphrasing what is kept verbatim on purpose).
    let mut head = user_text("task");
    head.content
        .push(Block::Text(format!("{SUMMARY_MARKER}\nold")));
    head.content
        .push(Block::Text(format!("{TODO_MARKER}\n[x] done")));
    head.content
        .push(Block::Text(format!("{USER_QUOTE_MARKER}\nsaid")));
    let msgs = vec![
        head,
        Message::assistant(vec![Block::Text("a1".into())]),
        user_text("raw"),
    ];
    let lines = window_lines(&msgs, 3).concat();
    assert!(lines.contains("task"));
    assert!(lines.contains("raw"));
    assert!(!lines.contains("old"), "no running summary in transcript");
    assert!(
        !lines.contains("done"),
        "no restored checklist in transcript"
    );
    assert!(!lines.contains("said"), "no verbatim quote in transcript");
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
        tool_absent_notes: Vec::new(),
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
        Self {
            name: name.into(),
            ran: None,
            cancel_on_run: None,
        }
    }
    fn tracking(name: &str, ran: Arc<AtomicBool>) -> Self {
        Self {
            name: name.into(),
            ran: Some(ran),
            cancel_on_run: None,
        }
    }
    fn cancel_on_run(name: &str, flag: Arc<AtomicBool>) -> Self {
        Self {
            name: name.into(),
            ran: None,
            cancel_on_run: Some(flag),
        }
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
    fn set_shutdown(&mut self, _shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.shutdown_calls.fetch_add(1, Ordering::Relaxed);
    }
}

/// A tool posing as one from an MCP server — exercises the agent-side
/// `mcp_server()` routing of `remove_mcp_server` without a live server
/// (the live handshake path is covered by the manual e2e).
struct FakeMcpTool {
    server: String,
    name: String,
}
impl Tool for FakeMcpTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: self.name.clone(),
            description: "fake mcp tool".into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }
    fn run(&self, _input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        Ok(ToolOutput::text("ok"))
    }
    fn mcp_server(&self) -> Option<&str> {
        Some(&self.server)
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
            // Leaf stand-in (default false): a plain tool reporting usage that
            // entered no counter anywhere — the agent's fold must share it.
            usage_in_shared_budget: false,
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
            // Faithful to the real `task` tool: the child's chats billed into
            // the shared counter while it ran, so the parent's fold must take
            // the totals WITHOUT re-sharing.
            usage_in_shared_budget: true,
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
        Self {
            name: "task",
            ms: 500,
            concurrency: ToolConcurrency::Parallel,
        }
    }
    /// A third-party tool shape: arbitrary name, sleep, and class.
    fn custom(name: &'static str, ms: u64, concurrency: ToolConcurrency) -> Self {
        Self {
            name,
            ms,
            concurrency,
        }
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
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            } => Some((*input_tokens, *output_tokens)),
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
        // Turn 2 reports no usage so the only input tokens are the two tasks'.
        mock::resp_text("final", None),
    ]);
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
    assert!(
        tags.contains(&"a".into()) && tags.contains(&"b".into()),
        "both ran: {tags:?}"
    );

    // Both tasks' ToolCall events still fire (now emitted from the batch
    // block after the gate, not the inline pass) — in block order a, b.
    let toolcalls: Vec<&Value> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCall { name, input } if name == "task" => Some(input),
            _ => None,
        })
        .collect();
    assert_eq!(
        toolcalls.len(),
        2,
        "both task ToolCalls must fire: {toolcalls:?}"
    );
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
        vec![Box::new(SlowTool::custom(
            "worker",
            500,
            ToolConcurrency::Parallel,
        ))],
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
        vec![Box::new(SlowTool::custom(
            "slowseq",
            400,
            ToolConcurrency::Sequential,
        ))],
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
                Block::ToolUse {
                    id: "e1".into(),
                    name: "first".into(),
                    input: json!({}),
                },
                Block::ToolUse {
                    id: "p".into(),
                    name: "task".into(),
                    input: json!({"label": "m"}),
                },
                Block::ToolUse {
                    id: "e2".into(),
                    name: "second".into(),
                    input: json!({}),
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
            Block::ToolUse {
                id: "a".into(),
                name: "trip".into(),
                input: json!({}),
            },
            Block::ToolUse {
                id: "b".into(),
                name: "task".into(),
                input: json!({"label": "x"}),
            },
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
    let o = agent
        .run(&mut |e| match &e {
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
    fn after_tool(
        &self,
        tool: &str,
        _input: &Value,
        output: &ToolOutput,
        executed: bool,
        _ctx: &ToolCtx,
    ) {
        self.calls.lock().unwrap().push(format!(
            "after:{tool}:{executed}:{err}:{content}",
            err = output.is_error,
            content = output.content
        ));
    }
    fn on_chat_request(&self, req: &ChatRequest) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("chat:{}", req.model));
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
        mock::resp_tool(
            "t1",
            "danger",
            json!({"cmd": "rm -rf /"}),
            mock::mk_usage(5, 5),
        ),
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

    assert!(
        !ran.load(Ordering::Relaxed),
        "a denied tool must never execute"
    );
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
fn hooks_deny_reason_respects_the_output_budget() {
    // External review: a host's deny reason is model-visible is_error text
    // and can be arbitrarily long — run_tool truncates MCP error strings to
    // ctx.max_output_chars for exactly this reason, but the three Deny
    // sites (sequential gate, parallel gate, run_code inner gate) used to
    // pass the reason through verbatim. A 5000-char denial must come back
    // truncated with the omission marker, in records AND the pairing
    // tool_result the model reads.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![
        mock::resp_tool("t1", "danger", json!({}), mock::mk_usage(5, 5)),
        mock::resp_text("final", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(EchoTool::new("danger"))],
        vec![user_text("go")],
        test_ctx(), // max_output_chars = 1000
    );
    let long_reason = "x".repeat(5000);
    agent.set_hooks(Arc::new(CaptureHooks::deny("danger", &long_reason)));
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();

    assert!(o.tool_calls[0].is_error);
    let out = &o.tool_calls[0].output;
    assert!(
        out.contains("[truncated, limit 1000 chars]"),
        "deny reason must be truncated to the ctx budget, got {} chars",
        out.chars().count()
    );
    assert!(out.chars().count() < 1100, "nowhere near the original 5000");
}

#[test]
fn session_save_load_does_not_carry_hooks() {
    // The hooks layer lives on the Agent instance, never in the session
    // file (hooks.rs Edges: an agent rebuilt from --resume does NOT carry
    // hooks over). Pin the contract end to end: a set_hooks'd agent that
    // saves + reloads through the real session API yields a rebuilt agent
    // with NO hooks — resume cannot accidentally inherit a policy, and
    // re-installing is on the caller.
    let cfg = cfg_with(2, 0, 0);
    let (prov, _) = MockProvider::new(vec![mock::resp_text("hi", None)]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("x")],
        test_ctx(),
    );
    assert!(agent.ctx.hooks.is_none(), "assembly leaves hooks unset");
    agent.set_hooks(Arc::new(CaptureHooks::new()));
    assert!(agent.ctx.hooks.is_some());

    // Save + rebuild exactly like the --resume path: fresh agent, the
    // session file's messages handed back in.
    let path = std::env::temp_dir().join(format!("ta-hooks-resume-{}.json", std::process::id()));
    crate::session::save(&path, "m", agent.messages()).unwrap();
    drop(agent);
    let (_, msgs) = crate::session::load(&path)
        .unwrap()
        .expect("saved session loads");
    let (prov2, _) = MockProvider::new(vec![mock::resp_text("y", None)]);
    let rebuilt = Agent::new(&cfg, Box::new(prov2), vec![], msgs, test_ctx());
    assert!(
        rebuilt.ctx.hooks.is_none(),
        "resume rebuild carries NO hooks — set_hooks again is on the caller"
    );
    let _ = std::fs::remove_file(&path);
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
    let hooks = Arc::new(CaptureHooks::deny(
        "task",
        "no sub-agents in this deployment",
    ));
    agent.set_hooks(hooks.clone());
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();

    assert!(
        events
            .iter()
            .all(|e| !matches!(e, AgentEvent::SubAgent { .. })),
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
    let chats: Vec<&str> = calls
        .iter()
        .filter_map(|c| c.strip_prefix("chat:"))
        .collect();
    assert_eq!(
        chats.len(),
        3,
        "one on_chat_request per main turn: {calls:?}"
    );
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
    let chats: Vec<&str> = calls
        .iter()
        .filter_map(|c| c.strip_prefix("chat:"))
        .collect();
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
                Block::ToolUse {
                    id: "c".into(),
                    name: "task".into(),
                    input: json!({"label": "c"}),
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

/// A hooks layer that sets the run's cancel flag INSIDE `before_tool`, then
/// Allows — the external re-review P2 repro: an approval dialog that was
/// still open when the user hit Esc. The gate returns "run it" while the run
/// has already committed to stopping; the gate→execution checkpoint must
/// catch the contradiction.
struct CancelDuringApproval {
    flag: Arc<AtomicBool>,
    gates: Arc<std::sync::atomic::AtomicUsize>,
    after: std::sync::Mutex<Vec<String>>,
}
impl Hooks for CancelDuringApproval {
    fn before_tool(&self, _tool: &str, _input: &Value, _ctx: &ToolCtx) -> ToolDecision {
        self.gates.fetch_add(1, Ordering::Relaxed);
        self.flag.store(true, Ordering::Relaxed);
        ToolDecision::Allow
    }
    fn after_tool(
        &self,
        tool: &str,
        _input: &Value,
        output: &ToolOutput,
        executed: bool,
        _ctx: &ToolCtx,
    ) {
        self.after
            .lock()
            .unwrap()
            .push(format!("{tool}:{executed}:{}", output.content));
    }
}

#[test]
fn cancel_during_approval_gate_does_not_execute_sequential_tool() {
    // The inline path: gate (sets cancel) → Allow → checkpoint refuses. The
    // tool never runs, the slot carries the same "[cancelled by user]" text
    // the Cancelled-slot synthesis uses, `after_tool` reports executed=false
    // (Deny symmetry), and the run ends "cancelled" — no turn-2 request.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![
        mock::resp_tool("t1", "danger", json!({"cmd": "x"}), None),
        mock::resp_text("never reached", None),
    ]);
    let ran = Arc::new(AtomicBool::new(false));
    let flag = Arc::new(AtomicBool::new(false));
    let hooks = Arc::new(CancelDuringApproval {
        flag: flag.clone(),
        gates: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        after: std::sync::Mutex::new(Vec::new()),
    });
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(EchoTool::tracking("danger", ran.clone()))],
        vec![user_text("go")],
        test_ctx(),
    );
    agent.set_hooks(hooks.clone());
    agent.set_cancel(flag);
    let o = agent.run(&mut |_| {}).unwrap();
    assert!(
        !ran.load(Ordering::Relaxed),
        "an Allowed call must not execute once cancel landed"
    );
    assert_eq!(o.stop_reason, "cancelled");
    assert_eq!(
        agent.trailing_tool_result_contents(),
        vec!["[cancelled by user]"],
        "the slot reports the cancel vocabulary, N:N intact"
    );
    assert_eq!(
        *hooks.after.lock().unwrap(),
        vec!["danger:false:[cancelled by user]".to_string()],
        "after_tool fires with executed=false, Deny symmetry"
    );
}

#[test]
fn cancel_during_approval_gate_does_not_execute_parallel_batch() {
    // The batch path: the gate loop approves the only Parallel call while
    // setting cancel — the pre-spawn checkpoint must skip the whole batch.
    // The approved call gets the placeholder loop's recorded
    // after_tool(executed=false) slot (its before_tool fired, so the audit
    // trail must close the pair — external re-review P3): same shape as the
    // inline gate checkpoint, no worker ever ran.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![
        mock::resp_tool("p1", "task", json!({"label": "p"}), None),
        mock::resp_text("never reached", None),
    ]);
    let flag = Arc::new(AtomicBool::new(false));
    let hooks = Arc::new(CancelDuringApproval {
        flag: flag.clone(),
        gates: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        after: std::sync::Mutex::new(Vec::new()),
    });
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(TaskTool)],
        vec![user_text("go")],
        test_ctx(),
    );
    agent.set_hooks(hooks.clone());
    agent.set_cancel(flag);
    // Events are collected too: the batch announced its ToolCall before the
    // gates ran, so the cancel-synthesized slot owes the stream a matching
    // ToolResult — a consumer must never see an unclosed pair (external
    // re-review P2).
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let o = agent
        .run(&mut move |ev| {
            let tag = match &ev {
                AgentEvent::ToolCall { .. } => "call",
                AgentEvent::ToolResult { .. } => "result",
                _ => "other",
            };
            seen2.lock().unwrap().push(tag.into());
        })
        .unwrap();
    assert_eq!(o.stop_reason, "cancelled");
    assert_eq!(o.tool_calls.len(), 1, "the approved call is recorded");
    assert_eq!(o.tool_calls[0].output, "[cancelled by user]");
    assert!(o.tool_calls[0].is_error);
    assert_eq!(
        agent.trailing_tool_result_contents(),
        vec!["[cancelled by user]"],
        "placeholder synthesis keeps N:N pairing"
    );
    assert_eq!(
        *hooks.after.lock().unwrap(),
        vec!["task:false:[cancelled by user]".to_string()],
        "a passed gate never leaves an unmatched before_tool"
    );
    let seen = seen.lock().unwrap();
    let calls = seen.iter().filter(|t| *t == "call").count();
    let results = seen.iter().filter(|t| *t == "result").count();
    assert_eq!(
        (calls, results),
        (1, 1),
        "the announced ToolCall got its cancel ToolResult: {seen:?}"
    );
}

/// A Parallel stand-in that flips the shared cancel flag the moment its tool
/// body runs, and counts how many bodies ran — exercises the wave-boundary
/// cancel checkpoint (external re-review P2): with 9 calls and an 8-wide
/// wave, the cancel tripped during wave 1 must keep wave 2 (call #9) from
/// ever starting.
struct CancellingTaskTool {
    flag: Arc<AtomicBool>,
    runs: Arc<std::sync::atomic::AtomicUsize>,
}
impl Tool for CancellingTaskTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "task".into(),
            description: "test parallel tool that trips cancel mid-wave".into(),
            schema: json!({"type": "object", "properties": {"label": {"type": "string"}}}),
        }
    }
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Parallel
    }
    fn run(&self, _input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        self.runs.fetch_add(1, Ordering::Relaxed);
        self.flag.store(true, Ordering::Relaxed);
        Ok(ToolOutput::text("ok"))
    }
}

/// Nine tool_use blocks in ONE turn (id t0..t8) — one more than the wave cap.
fn nine_task_turn() -> MockTurn {
    MockTurn::Ok(ChatResponse {
        blocks: (0..9)
            .map(|k| Block::ToolUse {
                id: format!("t{k}"),
                name: "task".into(),
                input: json!({"label": k}),
            })
            .collect(),
        stop_reason: "tool_use".into(),
        usage: None,
    })
}

#[test]
fn cancel_between_waves_never_starts_the_next_wave() {
    // 9 parallel calls, wave cap 8 → 2 waves. Wave-1 workers trip cancel from
    // inside the tool body; the wave-start checkpoint must keep wave 2 from
    // spawning (before the fix, call #9 still entered the tool and the run
    // then reported cancelled). How many wave-1 workers pass the worker-side
    // pre-check is racy by design — the only hard bounds are 1..=8.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![nine_task_turn(), mock::resp_text("unreached", None)]);
    let flag = Arc::new(AtomicBool::new(false));
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(CancellingTaskTool {
            flag: flag.clone(),
            runs: runs.clone(),
        })],
        vec![user_text("go")],
        test_ctx(),
    );
    agent.set_cancel(flag);
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let o = agent
        .run(&mut move |ev| {
            let tag = match &ev {
                AgentEvent::ToolCall { .. } => "call",
                AgentEvent::ToolResult { .. } => "result",
                _ => "other",
            };
            seen2.lock().unwrap().push(tag.into());
        })
        .unwrap();
    assert_eq!(o.stop_reason, "cancelled");
    let n = runs.load(Ordering::Relaxed);
    assert!(
        (1..9).contains(&n),
        "wave 2 must never start: expected 1..=8 tool bodies, got {n}"
    );
    let seen = seen.lock().unwrap();
    let calls = seen.iter().filter(|t| *t == "call").count();
    let results = seen.iter().filter(|t| *t == "result").count();
    assert_eq!(
        (calls, results),
        (9, 9),
        "every announced call closed — ran or cancel-synthesized: {seen:?}"
    );
    // Audit unification (external re-review P3): call #9 was approved by the
    // gate but never started (wave-2 cancel checkpoint) — it still lands in
    // the records as an executed=false-style placeholder, so every announced
    // + gated call is accounted for.
    assert_eq!(
        o.tool_calls.len(),
        9,
        "8 wave-1 slots (ran or pre-body-cancelled) + 1 wave-2 placeholder"
    );
    assert_eq!(o.tool_calls[8].output, "[cancelled by user]");
    assert!(o.tool_calls[8].is_error);
}

/// A Parallel tool flooding the event funnel (more events than the bounded
/// channel holds) — the host sink panics on the first one, reproducing the
/// external re-review P2 deadlock: with the receiver borrowed from outside
/// the scope, workers blocked on the full funnel forever. `saw_cancel` pins
/// the CancelOnUnwind guard: without it nothing would ever set the shared
/// flag, and in-flight workers would flood all 3000 iterations into the
/// void instead of winding down.
struct FloodTool {
    saw_cancel: Arc<AtomicBool>,
}
impl Tool for FloodTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "task".into(),
            description: "test parallel tool flooding the event funnel".into(),
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
        _input: &Value,
        ctx: &ToolCtx,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<ToolOutput> {
        for k in 0..3000 {
            emit(AgentEvent::SubAgent {
                name: "flood".into(),
                event: Box::new(AgentEvent::TextDelta(format!("chunk-{k}"))),
            });
            if ctx_cancelled(ctx) {
                self.saw_cancel.store(true, Ordering::Relaxed);
                break;
            }
        }
        Ok(ToolOutput::text("flooded"))
    }
}

#[test]
fn parallel_event_sink_panic_unwinds_instead_of_deadlocking() {
    // Host callback panics on the first flooded event while workers keep
    // sending. The scope must unwind (receiver dropped as a closure local +
    // cancel guard), not hang on the full funnel. A watchdog turns a
    // regression back into a test failure instead of a hung suite. The
    // saw_cancel assertion pins the guard itself: delete the CancelOnUnwind
    // struct and this test still unwinds (rx drop unblocks the sends) but
    // no worker ever observes a cancel.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![
        MockTurn::Ok(ChatResponse {
            blocks: (0..2)
                .map(|k| Block::ToolUse {
                    id: format!("t{k}"),
                    name: "task".into(),
                    input: json!({"label": k}),
                })
                .collect(),
            stop_reason: "tool_use".into(),
            usage: None,
        }),
        mock::resp_text("unreached", None),
    ]);
    let saw_cancel = Arc::new(AtomicBool::new(false));
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(FloodTool {
            saw_cancel: saw_cancel.clone(),
        })],
        vec![user_text("go")],
        test_ctx(),
    );
    // The guard writes through ctx.cancel — install a flag or it no-ops.
    agent.set_cancel(Arc::new(AtomicBool::new(false)));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut sink = |ev: AgentEvent| {
                if matches!(ev, AgentEvent::SubAgent { .. }) {
                    panic!("host event callback blew up");
                }
            };
            let _ = agent.run(&mut sink);
        }));
        let _ = done_tx.send(r.is_err());
    });
    match done_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(panicked) => assert!(panicked, "the host panic must propagate"),
        Err(_) => panic!("deadlock: workers still blocked on the event funnel"),
    }
    assert!(
        saw_cancel.load(Ordering::Relaxed),
        "the unwind guard must trip the shared cancel so in-flight workers wind down"
    );
}

/// A Parallel tool whose body panics on demand — pins the join arm that
/// converts a panicked worker into an error slot instead of losing the
/// batch (test-audit gap: deleting the `unwrap_or_else` arm used to keep
/// the whole suite green).
struct PanicOnMarkTool;
impl Tool for PanicOnMarkTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "task".into(),
            description: "test parallel tool that panics on a marked input".into(),
            schema: json!({"type": "object", "properties": {"label": {"type": "string"}}}),
        }
    }
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Parallel
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let label = input["label"].as_str().unwrap_or("?");
        if label == "boom" {
            // Model `bill_chat` having already billed this usage: share into
            // the run-wide counter (real when the test installs one — the
            // nested-panic tests below read it back) and record into the
            // probe. The probe is the only channel that survives the unwind,
            // and the join arm must recover it into the totals; the recovered
            // output carries usage_in_shared_budget=true so the fold will not
            // re-share.
            let u = Usage {
                input_tokens: 100,
                output_tokens: 20,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            };
            if let Some(g) = &ctx.budget {
                g.share(u.total());
            }
            crate::usage_probe::record(&u);
            panic!("worker body blew up");
        }
        Ok(ToolOutput::text(format!("ok-{label}")))
    }
}

#[test]
fn parallel_worker_panic_becomes_an_error_slot_and_siblings_survive() {
    // 3 parallel calls in one turn; the middle worker's body panics. The
    // run must complete: the panicked call lands as an is_error slot, both
    // siblings join normally, and N:N history + records stay complete.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![
        MockTurn::Ok(ChatResponse {
            blocks: ["a", "boom", "c"]
                .iter()
                .enumerate()
                .map(|(k, label)| Block::ToolUse {
                    id: format!("t{k}"),
                    name: "task".into(),
                    input: json!({"label": label}),
                })
                .collect(),
            stop_reason: "tool_use".into(),
            usage: None,
        }),
        mock::resp_text("done", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(PanicOnMarkTool)],
        vec![user_text("go")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(o.tool_calls.len(), 3, "every announced call is recorded");
    assert_eq!(o.tool_calls[0].output, "ok-a");
    assert!(!o.tool_calls[0].is_error);
    assert_eq!(o.tool_calls[1].output, "parallel tool worker panicked");
    assert!(o.tool_calls[1].is_error);
    assert_eq!(o.tool_calls[2].output, "ok-c");
    assert_eq!(
        (o.input_tokens, o.output_tokens),
        (100, 20),
        "usage billed before the worker panicked must still reach the totals \
         (recovered through the per-worker probe)"
    );
    let tool_msg = agent
        .messages()
        .iter()
        .find(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, Block::ToolResult { .. }))
        })
        .expect("the tool_result user message");
    let contents: Vec<&str> = tool_msg
        .content
        .iter()
        .filter_map(|b| match b {
            Block::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        contents,
        vec!["ok-a", "parallel tool worker panicked", "ok-c"],
        "N:N pairing intact across a panicked worker"
    );
}

// ── nested-parallel panic: the child's FULL ledger must reach its probe ──
//
// The `usage_probe` edge this fixture pins: a `task` child (worker thread C)
// runs its OWN parallel batch; a nested worker panics; the recovered usage is
// folded into C's ledger at the records.push point — and THEN C itself
// panics. Without a fold-point `usage_probe::record`, C's probe only ever
// held C's direct chats: the folded tool usage (normal outputs and recovered
// ones alike) died with the thread, and the parent's recovered totals
// diverged from the shared counter.

/// A Parallel tool that reports a fixed `Usage` in a successful output with
/// `usage_in_shared_budget: false` — the "host CallbackTool metering an
/// upstream API" shape: the fold shares it into the counter and (the fix
/// under test) mirrors it into the thread probe.
struct BilledUsageTool {
    usage: Usage,
}
impl Tool for BilledUsageTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "billed".into(),
            description: "test tool reporting fixed usage".into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Parallel
    }
    fn run(&self, _input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        Ok(ToolOutput {
            content: "billed".into(),
            image: None,
            is_error: false,
            usage: Some(self.usage),
            usage_in_shared_budget: false,
            meta: None,
        })
    }
}

/// A Sequential tool that panics on the AGENT'S OWN thread (inline pass) —
/// the escape vehicle for the nested-panic scenario. A Parallel tool's panic
/// can never escape an agent's `run`: that agent's own join arm contains it
/// (the very mechanism under test), so blowing up the child's thread needs an
/// inline call.
struct PanicInlineTool;
impl Tool for PanicInlineTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "panic_inline".into(),
            description: "test tool that panics inline".into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }
    fn run(&self, _input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        panic!("inline blow-up escapes the agent's own run");
    }
}

/// A Parallel `task`-shaped tool that assembles a REAL child Agent (own
/// MockProvider + toolset, fleet budget / cancel / sandbox shared through the
/// parent's ctx) and drives it with `run_as_subagent` — the production
/// SubAgent assembly minus the HTTP provider build. The outcome maps to a
/// ToolOutput exactly like `tools::subagent::outcome_to_output` (private
/// there): final text + usage billed to the parent with
/// `usage_in_shared_budget: true`. The child's script is picked by the
/// `label` input so one parent turn can fan out differently-behaving
/// children.
struct NestedTaskTool;
impl Tool for NestedTaskTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "task".into(),
            description: "test nested sub-agent tool".into(),
            schema: json!({"type": "object", "properties": {"label": {"type": "string"}}}),
        }
    }
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Parallel
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let label = input["label"].as_str().unwrap_or("?").to_string();
        let (turns, tools) = nested_child_script(&label);
        let mut child_cfg = cfg_with(5, 0, 0);
        child_cfg.system_prompt = Some("nested test sub-agent".into());
        let (prov, _) = MockProvider::new(turns);
        let mut child = Agent::new(
            &child_cfg,
            Box::new(prov),
            tools,
            vec![user_text("go")],
            // Mirror child_ctx: sandbox inherited, fleet budget + deadline
            // Arc-shared, sub-agent identity tagged.
            ToolCtx {
                tool_absent_notes: ctx.tool_absent_notes.clone(),
                workdir: ctx.workdir.clone(),
                max_output_chars: ctx.max_output_chars,
                restrict_to_workdir: ctx.restrict_to_workdir,
                cancel: ctx.cancel.clone(),
                shell_timeout_secs: ctx.shell_timeout_secs,
                hooks: ctx.hooks.clone(),
                subagent: Some(label.clone()),
                budget: ctx.budget.clone(),
                deadline: ctx.deadline,
            },
        );
        if let Some(flag) = ctx.cancel.clone() {
            child.set_cancel(flag);
        }
        match child.run_as_subagent(&mut |_| {}) {
            Ok(o) => Ok(ToolOutput {
                content: o.final_text.clone(),
                image: None,
                is_error: false,
                usage: Some(Usage {
                    input_tokens: o.input_tokens,
                    output_tokens: o.output_tokens,
                    cache_creation_input_tokens: o.cache_creation_input_tokens,
                    cache_read_input_tokens: o.cache_read_input_tokens,
                }),
                usage_in_shared_budget: true,
                meta: None,
            }),
            Err(re) => {
                // Mirror run_subagent's honest-failure mapping: the partial
                // cost rides the error output (the child billed its own
                // chats into the inherited counter).
                let mut out = ToolOutput::error(format!("nested {label} failed: {re:#}"));
                if re.usage.total() > 0 {
                    out.usage = Some(re.usage);
                    out.usage_in_shared_budget = true;
                }
                Ok(out)
            }
        }
    }
}

/// Script + toolset for one nested child, keyed by label. Panic variants:
/// turn 1 = parallel batch [billed usage tool, PanicOnMarkTool] (that panic
/// is recovered into a synthetic error output and folded at the child's
/// records.push point), turn 2 = PanicOnMarkTool as a single call (same
/// recovery, folded again), turn 3 = PanicInlineTool — the Sequential escape
/// that panics on the child's OWN thread. Calm variants end after turn 2's
/// final text — the no-panic twin for the no-double-count regression.
fn nested_child_script(label: &str) -> (Vec<MockTurn>, Vec<Box<dyn Tool>>) {
    let usage = |i: u64, o: u64| Usage {
        input_tokens: i,
        output_tokens: o,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
    };
    let (u1, ut, u2, u3, panics) = match label {
        "boom-a" => ((10, 2), (30, 7), (20, 3), Some((30, 4)), true),
        "boom-b" => ((11, 3), (31, 8), (21, 4), Some((31, 5)), true),
        "calm-a" => ((10, 2), (30, 7), (20, 3), None, false),
        "calm-b" => ((11, 3), (31, 8), (21, 4), None, false),
        _ => unreachable!("test-only labels"),
    };
    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(BilledUsageTool {
            usage: usage(ut.0, ut.1),
        }),
        Box::new(PanicOnMarkTool),
    ];
    let tool_turn = |blocks: Vec<Block>, u: Option<Usage>| {
        MockTurn::Ok(ChatResponse {
            blocks,
            stop_reason: "tool_use".into(),
            usage: u,
        })
    };
    let mut turns = vec![tool_turn(
        vec![
            Block::ToolUse {
                id: "c1".into(),
                name: "billed".into(),
                input: json!({}),
            },
            Block::ToolUse {
                id: "c2".into(),
                name: "task".into(),
                input: json!({"label": "boom"}),
            },
        ],
        Some(usage(u1.0, u1.1)),
    )];
    if panics {
        // Single-call batch: the child's own join arm recovers the panic
        // (usage_in_shared_budget=true) and folds it — the exact "nested
        // recovery folded into the child's ledger" shape under test.
        turns.push(tool_turn(
            vec![Block::ToolUse {
                id: "c3".into(),
                name: "task".into(),
                input: json!({"label": "boom"}),
            }],
            Some(usage(u2.0, u2.1)),
        ));
        turns.push(tool_turn(
            vec![Block::ToolUse {
                id: "c4".into(),
                name: "panic_inline".into(),
                input: json!({}),
            }],
            u3.map(|(i, o)| usage(i, o)),
        ));
        tools.push(Box::new(PanicInlineTool));
    } else {
        turns.push(mock::resp_text("all done", Some(usage(u2.0, u2.1))));
    }
    (turns, tools)
}

#[test]
fn nested_parallel_panic_probe_carries_the_childs_full_ledger() {
    // Two parallel `task` calls; both children run the nested-panic script
    // and their threads blow up on the inline escape. The parent's join arm
    // must recover each child's FULL ledger — before the fold-point record
    // the probes held only the children's direct chats (boom-a (60,9) /
    // boom-b (63,12)); the billed tool's fold and both recovered panics' folds
    // died with the threads.
    let mut cfg = cfg_with(5, 0, 0);
    // max_total_tokens > 0 makes the top-level run install the REAL fleet
    // counter (fresh per run, read back off agent.ctx below) — the
    // counter-vs-totals equality half of the assertion.
    cfg.limits.max_total_tokens = 1_000_000;
    let (prov, _) = MockProvider::new(vec![
        MockTurn::Ok(ChatResponse {
            blocks: ["boom-a", "boom-b"]
                .iter()
                .enumerate()
                .map(|(k, label)| Block::ToolUse {
                    id: format!("t{k}"),
                    name: "task".into(),
                    input: json!({"label": label}),
                })
                .collect(),
            stop_reason: "tool_use".into(),
            usage: mock::mk_usage(1000, 100),
        }),
        mock::resp_text("done", mock::mk_usage(500, 50)),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(NestedTaskTool)],
        vec![user_text("go")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(o.tool_calls.len(), 2, "both task calls recorded");
    for rec in &o.tool_calls {
        assert_eq!(rec.output, "parallel tool worker panicked");
        assert!(rec.is_error);
    }
    // Per-child ledger reaching the parent's probe (LLM chats via bill_chat +
    // the billed tool's fold + both recovered panics' folds):
    //   boom-a: (10+30+100+20+100+30, 2+7+20+3+20+4) = (290, 56)
    //   boom-b: (11+31+100+21+100+31, 3+8+20+4+20+5) = (294, 60)
    // Parent totals = its own two chats + both recovered ledgers.
    assert_eq!(
        (o.input_tokens, o.output_tokens),
        (1000 + 500 + 290 + 294, 100 + 50 + 56 + 60),
        "recovered child usage must include the fold-point records"
    );
    // No divergence: every token entered the shared counter exactly once —
    // child LLM chats via bill_chat, the billed tool's usage at the child's
    // fold (usage_in_shared_budget=false), each panic's usage shared by the
    // tool itself before recording; recovered outputs (true) are totals-only
    // at every fold. The counter accumulates Usage::total() = in + out here
    // (cache axes are 0 throughout).
    let used = agent
        .ctx
        .budget
        .as_ref()
        .expect("fleet counter installed by the top-level run")
        .used
        .load(Ordering::Relaxed);
    assert_eq!(
        used,
        2084 + 266,
        "counter delta == totals delta (no divergence)"
    );
}

#[test]
fn nested_parallel_normal_child_completion_bills_exactly_once() {
    // The no-panic twin: same nested shape, but each child rides out its
    // contained worker panic and completes normally. The fold-point records
    // then land in a probe that is DISCARDED on a successful join (the
    // ToolOutput is the only authority) — exact totals + counter equality pin
    // that the added recording never double-counts.
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.max_total_tokens = 1_000_000;
    let (prov, _) = MockProvider::new(vec![
        MockTurn::Ok(ChatResponse {
            blocks: ["calm-a", "calm-b"]
                .iter()
                .enumerate()
                .map(|(k, label)| Block::ToolUse {
                    id: format!("t{k}"),
                    name: "task".into(),
                    input: json!({"label": label}),
                })
                .collect(),
            stop_reason: "tool_use".into(),
            usage: mock::mk_usage(1000, 100),
        }),
        mock::resp_text("done", mock::mk_usage(500, 50)),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(NestedTaskTool)],
        vec![user_text("go")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(o.tool_calls.len(), 2, "both task calls recorded");
    for rec in &o.tool_calls {
        assert_eq!(rec.output, "all done", "child summary rides through");
        assert!(!rec.is_error);
    }
    // Per-child ledger: LLM chats + billed tool fold + recovered panic fold:
    //   calm-a: (10+30+100+20, 2+7+20+3) = (160, 32)
    //   calm-b: (11+31+100+21, 3+8+20+4) = (163, 35)
    assert_eq!(
        (o.input_tokens, o.output_tokens),
        (1000 + 500 + 160 + 163, 100 + 50 + 32 + 35),
        "normal completion bills the same totals with and without the probe"
    );
    let used = agent
        .ctx
        .budget
        .as_ref()
        .expect("fleet counter installed by the top-level run")
        .used
        .load(Ordering::Relaxed);
    assert_eq!(
        used,
        1823 + 217,
        "counter delta == totals delta (no divergence)"
    );
}

#[test]
fn cancel_landed_mid_gate_loop_skips_the_remaining_gates() {
    // 2 parallel calls in one turn; cancel lands inside call #1's approval.
    // The per-call gate checkpoint must keep call #2 from ever reaching
    // before_tool (no approval prompt for a run that is already stopping).
    // Call #1 — approved but never started — becomes the recorded
    // placeholder; call #2 — never gated — stays Cancelled: no record and
    // no after_tool, but (announced) still gets its synthesized ToolResult
    // event. Deleting the gate loop's `if self.cancelled() { break; }` used
    // to keep the suite green (test-audit gap).
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![
        MockTurn::Ok(ChatResponse {
            blocks: (0..2)
                .map(|k| Block::ToolUse {
                    id: format!("t{k}"),
                    name: "task".into(),
                    input: json!({"label": k}),
                })
                .collect(),
            stop_reason: "tool_use".into(),
            usage: None,
        }),
        mock::resp_text("unreached", None),
    ]);
    let flag = Arc::new(AtomicBool::new(false));
    let gates = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hooks = Arc::new(CancelDuringApproval {
        flag: flag.clone(),
        gates: gates.clone(),
        after: std::sync::Mutex::new(Vec::new()),
    });
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(TaskTool)],
        vec![user_text("go")],
        test_ctx(),
    );
    agent.set_hooks(hooks.clone());
    agent.set_cancel(flag);
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let o = agent
        .run(&mut move |ev| {
            let tag = match &ev {
                AgentEvent::ToolCall { .. } => "call",
                AgentEvent::ToolResult { .. } => "result",
                _ => "other",
            };
            seen2.lock().unwrap().push(tag.into());
        })
        .unwrap();
    assert_eq!(o.stop_reason, "cancelled");
    assert_eq!(
        gates.load(Ordering::Relaxed),
        1,
        "call #2 must never reach its gate once cancel landed"
    );
    assert_eq!(
        o.tool_calls.len(),
        1,
        "only the approved call is recorded (placeholder)"
    );
    assert_eq!(o.tool_calls[0].output, "[cancelled by user]");
    assert_eq!(
        *hooks.after.lock().unwrap(),
        vec!["task:false:[cancelled by user]".to_string()],
        "the approved call gets after_tool(executed=false); the never-gated one gets nothing"
    );
    let seen = seen.lock().unwrap();
    let calls = seen.iter().filter(|t| *t == "call").count();
    let results = seen.iter().filter(|t| *t == "result").count();
    assert_eq!(
        (calls, results),
        (2, 2),
        "both announced calls closed — placeholder + Cancelled synthesis: {seen:?}"
    );
}

/// A turn of `n` identical Parallel `task` calls (same input → same
/// output), for the stuck-detector-in-the-batch-path tests.
fn identical_task_turn(n: usize, label: &str) -> MockTurn {
    MockTurn::Ok(ChatResponse {
        blocks: (0..n)
            .map(|k| Block::ToolUse {
                id: format!("t{k}"),
                name: "task".into(),
                input: json!({"label": label}),
            })
            .collect(),
        stop_reason: "tool_use".into(),
        usage: None,
    })
}

#[test]
fn stuck_detection_counts_parallel_batch_calls() {
    // 6 identical Parallel calls in ONE turn: the assembly-pass counter must
    // treat batch slots exactly like inline repeats — turn-end stuck stop.
    // (All seven pre-existing stuck tests drive the sequential path only.)
    let cfg = cfg_with(10, 0, 0);
    let (prov, _) = MockProvider::new(vec![
        identical_task_turn(6, "same"),
        mock::resp_text("unreached", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(TaskTool)],
        vec![user_text("go")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "stuck_loop");
    assert_eq!(o.turns, 1, "stopped at the first turn boundary");
    assert_eq!(o.tool_calls.len(), 6, "all six calls ran and are recorded");
}

#[test]
fn stuck_detection_parallel_batch_resets_on_a_differing_call() {
    // 5 identical Parallel calls + a 6th with a different input, all in one
    // batch: the streak breaks inside the batch, the run completes normally.
    let cfg = cfg_with(10, 0, 0);
    let mut turn = identical_task_turn(5, "same");
    if let MockTurn::Ok(resp) = &mut turn {
        resp.blocks.push(Block::ToolUse {
            id: "t5".into(),
            name: "task".into(),
            input: json!({"label": "other"}),
        });
    }
    let (prov, _) = MockProvider::new(vec![turn, mock::resp_text("done", None)]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(TaskTool)],
        vec![user_text("go")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(
        o.stop_reason, "end_turn",
        "a differing call breaks the streak"
    );
    assert_eq!(o.tool_calls.len(), 6);
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
                Block::ToolUse {
                    id: "a".into(),
                    name: "task".into(),
                    input: json!({"label": "a"}),
                },
                Block::ToolUse {
                    id: "e".into(),
                    name: "echo".into(),
                    input: json!({}),
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
fn run_with_preset_cancel_returns_immediately_zero_requests() {
    // Cancel-ownership contract (TinyWorker): run() NEVER clears the flag, so
    // a run started with cancel already set (a stop signalled during the
    // embedder's acceptance window — e.g. its lazy build) is honoured: the
    // loop-head check fires before turns/chat, giving stop_reason
    // "cancelled", turns == 0, zero provider requests, zero tokens.
    let cfg = cfg_with(5, 0, 0);
    let (prov, state) = MockProvider::new(vec![mock::resp_text("done", None)]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
    let flag = Arc::new(AtomicBool::new(true)); // stop already signalled
    agent.set_cancel(flag.clone());
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "cancelled");
    assert_eq!(o.turns, 0, "cancel must preempt the first turn entirely");
    assert_eq!(
        o.input_tokens
            + o.output_tokens
            + o.cache_creation_input_tokens
            + o.cache_read_input_tokens,
        0,
        "no provider call may be billed"
    );
    assert!(
        state.captured().is_empty(),
        "run must not consult the provider when cancel is preset"
    );
}

#[test]
fn run_does_not_clear_a_stale_cancel_flag() {
    // The old entry-point store(false) is GONE: a cancel that ended one run
    // persists into the next (level-triggered, embedder-owned). This locks
    // "run never clears" — the acceptance party (TUI input handler, retry
    // ladder, embedder) clears via clear_cancel instead. A second run
    // without a clear returns cancelled again with zero provider requests.
    let cfg = cfg_with(5, 0, 0);
    let (prov, state) = MockProvider::new(vec![mock::resp_text("done", None)]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
    let flag = Arc::new(AtomicBool::new(false));
    agent.set_cancel(flag.clone());
    flag.store(true, Ordering::Relaxed);
    let o1 = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o1.stop_reason, "cancelled");
    assert!(flag.load(Ordering::Relaxed), "run must leave the flag set");
    // No clear in between: the second run is cancelled too — deterministically,
    // not by racing an implicit reset.
    let o2 = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o2.stop_reason, "cancelled");
    assert_eq!(o2.turns, 0);
    assert!(
        state.captured().is_empty(),
        "neither run may consult the provider"
    );
}

#[test]
fn clear_cancel_lets_the_next_run_proceed() {
    // The acceptance-point remedy: after a cancelled run, the party accepting
    // the next task calls clear_cancel, and the following run executes
    // normally. This is the embedder-visible loop TinyWorker drives:
    // run (cancelled) → clear_cancel at send-message → run → completes.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![
        mock::resp_text("done", None),
        mock::resp_text("again", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
    let flag = Arc::new(AtomicBool::new(false));
    agent.set_cancel(flag.clone());
    // Stale stop from an earlier turn → this run is cancelled instantly.
    flag.store(true, Ordering::Relaxed);
    let o1 = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o1.stop_reason, "cancelled");
    // Acceptance point: clear, then the follow-up runs to completion.
    agent.clear_cancel();
    agent.push_user(vec![Block::Text("again".into())]);
    let o2 = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o2.stop_reason, "end_turn");
    assert_eq!(o2.turns, 1);
    assert!(!flag.load(Ordering::Relaxed));
    // clear_cancel is idempotent and safe without a flag installed.
    agent.clear_cancel();
}

#[test]
fn parallel_children_both_honour_shared_parent_cancel_none_clears_it() {
    // Acceptance #5: two sub-agent children share the parent's cancel Arc;
    // with the flag set, BOTH run_as_subagent calls return cancelled and
    // neither clears the flag (a sibling clearing it would drop the parent's
    // Ctrl-C for the whole batch). Since the contract change no entry point
    // clears at all — this pins that nothing regressed to per-run clearing.
    let flag = Arc::new(AtomicBool::new(true)); // parent Ctrl-C already fired
    let mk = || {
        let (prov, _) = MockProvider::new(vec![mock::resp_text("x", None)]);
        let mut a = Agent::new(
            &cfg_with(5, 0, 0),
            Box::new(prov),
            vec![],
            vec![user_text("hi")],
            test_ctx(),
        );
        a.set_cancel(flag.clone());
        a
    };
    let mut a = mk();
    let mut b = mk();
    let (oa, ob) = std::thread::scope(|s| {
        let ha = s.spawn(|| a.run_as_subagent(&mut |_| {}));
        let hb = s.spawn(|| b.run_as_subagent(&mut |_| {}));
        (
            ha.join().expect("child A panicked"),
            hb.join().expect("child B panicked"),
        )
    });
    assert_eq!(oa.unwrap().stop_reason, "cancelled");
    assert_eq!(ob.unwrap().stop_reason, "cancelled");
    assert!(
        flag.load(Ordering::Relaxed),
        "no child may clear the shared parent flag"
    );
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
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
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
fn summary_timeout_share_floors_both_branches_at_one_second() {
    // External re-review: the no-deadline branch divided STREAM_BACKSTOP by
    // the chunk count WITHOUT the 1s floor the deadline branch had — a
    // base64-heavy history chunked into more pieces than the budget has
    // seconds pushed each summary call's share under one second (a zero
    // Duration is an instant timeout), exactly when compression was most
    // needed. Extracted as a pure function so the floor is testable without
    // constructing a multi-million-char history.
    use std::time::{Duration, Instant};
    assert_eq!(summary_timeout_share(None, 1), STREAM_BACKSTOP);
    assert_eq!(
        summary_timeout_share(None, 10_000),
        Duration::from_secs(1),
        "no-deadline branch must floor at 1s regardless of chunk count"
    );
    let far = Instant::now() + Duration::from_secs(3600);
    assert_eq!(
        summary_timeout_share(Some(far), 10_000),
        Duration::from_secs(1)
    );
    // A far deadline with few calls keeps a generous share — the floor only
    // binds when the division would go below it.
    assert!(summary_timeout_share(Some(far), 2) > Duration::from_secs(60));
}

#[test]
fn run_as_subagent_does_not_reset_a_set_cancel_flag() {
    // The `task` sub-agent's child shares the parent's cancel Arc. Since the
    // cancel-ownership contract change NO entry point clears the flag (see
    // run()'s doc), so this is no longer a run-vs-subagent distinction — but
    // the invariant stays worth pinning directly: with the flag already set,
    // the sub-agent run honours it (stop_reason "cancelled") and leaves the
    // flag set. A regression to per-run clearing here would let a sibling
    // starting later in a parallel `task` batch drop a Ctrl-C signalled for
    // the whole batch.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![mock::resp_text("x", None)]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
    let flag = Arc::new(AtomicBool::new(true)); // already cancelled
    agent.set_cancel(flag.clone());
    let o = agent.run_as_subagent(&mut |_| {}).unwrap();
    assert_eq!(
        o.stop_reason, "cancelled",
        "sub-agent must honour a set flag, not reset it"
    );
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
    fn chat(
        &self,
        _req: &ChatRequest,
        on_event: &mut dyn FnMut(ChatEvent),
    ) -> Result<ChatResponse> {
        on_event(ChatEvent::TextDelta("partial".into()));
        // Simulate a Ctrl-C landing as the stream wraps up: the agent's own
        // cancel Arc flips true here — before the loop's abort check runs.
        // (A real cancel during the final streaming bytes hits the same
        // window: CancellableSse breaks the stream, chat() returns with
        // whatever stop_reason the provider last sent.)
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
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
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
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
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
fn hot_update_setters_take_effect_on_next_request() {
    // Pure field replacement: the swapped model/effort/budget must travel on
    // the next ChatRequest — the provider is stateless w.r.t. them, so no
    // rebuild is involved. Asserted against the mock's captured request, not
    // just the getters, so a regression that reads config instead of the
    // agent fields can't pass.
    let cfg = cfg_with(5, 0, 0);
    let (prov, state) = MockProvider::new(vec![mock::resp_text("ok", None)]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
    agent.set_model("other-model");
    agent.set_reasoning_effort(Some("high"));
    agent.set_max_tokens(7);
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(agent.model(), "other-model");
    let cap = state.captured();
    assert_eq!(cap[0].model, "other-model");
    assert_eq!(cap[0].max_tokens, 7);
    assert_eq!(cap[0].reasoning_effort.as_deref(), Some("high"));
}

#[test]
fn hot_update_setter_edges() {
    // max_tokens clamps to 1 (a persistent 0 would 400 every later request);
    // effort Some→None clears; set_model accepts whatever the host says
    // (validity fails honestly at the next request, like a config typo).
    let cfg = cfg_with(5, 0, 0);
    let (prov, state) = MockProvider::new(vec![
        mock::resp_text("ok", None),
        mock::resp_text("ok", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
    agent.set_max_tokens(0);
    agent.set_reasoning_effort(Some("low"));
    let _ = agent.run(&mut |_| {}).unwrap();
    agent.set_reasoning_effort(None);
    agent.set_model("");
    let _ = agent.run(&mut |_| {}).unwrap();
    let cap = state.captured();
    assert_eq!(cap[0].max_tokens, 1, "0 clamps to 1");
    assert_eq!(cap[0].reasoning_effort.as_deref(), Some("low"));
    assert_eq!(cap[1].reasoning_effort, None, "None clears the hint");
    assert_eq!(cap[1].model, "", "model passes through verbatim");
}

#[test]
fn set_max_context_tokens_switches_preflight_to_token_gate() {
    // A tiny ceiling set at runtime must arm the preflight estimate: the run
    // compresses (summary + main calls) even though the static config left
    // max_context_tokens at 0 (char heuristic, threshold 0 = never).
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.keep_recent_messages = 2;
    let initial = vec![
        user_text("task"),
        Message::assistant(vec![Block::Text("a1".into())]),
        user_text("u1"),
        Message::assistant(vec![Block::Text("a2".into())]),
        user_text("u2"),
    ];
    let (prov, state) = MockProvider::new(vec![
        mock::resp_text("SUMMARY", None),
        mock::resp_text("final answer", None),
    ]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    agent.set_max_context_tokens(1); // any history trips the 85% gate
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(state.calls(), 2, "summary + folded main request");
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Compressed {
            trigger: CompressTrigger::Preflight,
            ..
        }
    )));
}

#[test]
fn remove_mcp_server_routes_by_config_name_marker() {
    // Hot-remove routes by the mcp_server() marker (config name), taking the
    // whole server's tools and nothing else — the plain EchoTool (marker
    // None by trait default) must survive. Asserted on the NEXT REQUEST's
    // captured tool list, not just the getter, so a stale `tool_defs` mirror
    // can't pass.
    let cfg = cfg_with(5, 0, 0);
    let (prov, state) = MockProvider::new(vec![
        mock::resp_text("ok", None),
        mock::resp_text("ok", None),
    ]);
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(EchoTool::new("shell")),
        Box::new(FakeMcpTool {
            server: "db".into(),
            name: "mcp_db_query".into(),
        }),
        Box::new(FakeMcpTool {
            server: "db".into(),
            name: "mcp_db_exec".into(),
        }),
        Box::new(FakeMcpTool {
            server: "fs".into(),
            name: "mcp_fs_read".into(),
        }),
    ];
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        tools,
        vec![user_text("hi")],
        test_ctx(),
    );
    let _ = agent.run(&mut |_| {}).unwrap();
    let removed = agent.remove_mcp_server("db").unwrap();
    assert_eq!(removed, 2);
    let _ = agent.run(&mut |_| {}).unwrap();
    let cap = state.captured();
    assert_eq!(cap[0].tool_names.len(), 4, "before: all four tools sent");
    assert_eq!(
        cap[1].tool_names,
        vec!["shell".to_string(), "mcp_fs_read".to_string()],
        "after: db's tools gone from the request, fs and shell kept"
    );
}

#[test]
fn remove_mcp_server_unknown_name_is_err_and_keeps_tools() {
    // A name the agent has no tools from (typo / never added) must be a loud
    // Err, never a silent no-op the host mistakes for success.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![]);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(EchoTool::new("shell"))];
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        tools,
        vec![user_text("hi")],
        test_ctx(),
    );
    let before = agent.tool_names();
    let err = agent.remove_mcp_server("ghost").unwrap_err();
    assert!(format!("{err:#}").contains("ghost"));
    assert_eq!(agent.tool_names(), before, "failed remove mutates nothing");
}

#[test]
fn add_mcp_server_failed_handshake_leaves_agent_unchanged() {
    // Hermetic add-path guard: a server that can't even spawn (nonexistent
    // binary) is a recoverable Err with NO partial mutation — the agent's
    // tool list before and after is identical. The successful path (spawn +
    // handshake + tools on the next request + collision rollback) is the
    // manual e2e against examples/mcp_echo.py.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(EchoTool::new("shell"))],
        vec![user_text("hi")],
        test_ctx(),
    );
    let before = agent.tool_names();
    let bad = McpServerConfig {
        name: "nope".into(),
        command: Some("/nonexistent-tinyagent-test-mcp".into()),
        args: vec![],
        url: None,
        transport: None,
        headers: Default::default(),
        env: Default::default(),
    };
    assert!(agent.add_mcp_server(bad).is_err());
    assert_eq!(agent.tool_names(), before, "failed add mutates nothing");
}

/// 真实热增删端到端：起 examples/mcp_echo.py（本地 stdio MCP server，无网络），
/// 覆盖「spawn → 握手 → 工具进下一请求 → 真实 tools/call → 冲突回滚 →
/// 移除后 child 被同步回收」全链路——hermetic 用例只到路由与错误形状为止。
/// `#[ignore]`：需要 python3，不进常规 CI；
/// `cargo test hot_add_remove -- --ignored --nocapture`。
#[test]
#[ignore]
fn hot_add_remove_mcp_server_live() {
    let echo = McpServerConfig {
        name: "e1".into(),
        command: Some("python3".into()),
        args: vec![format!(
            "{}/../../examples/mcp_echo.py",
            env!("CARGO_MANIFEST_DIR")
        )],
        url: None,
        transport: None,
        headers: Default::default(),
        env: Default::default(),
    };
    let cfg = cfg_with(5, 0, 0);
    let (prov, state) = MockProvider::new(vec![
        mock::resp_tool("c1", "mcp_e1_echo", json!({"text": "hi"}), None),
        mock::resp_text("done", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
    assert!(agent.tool_names().is_empty());

    // Hot-add: handshake runs inside the call; the tool lands on the agent.
    let added = agent.add_mcp_server(echo.clone()).unwrap();
    assert_eq!(added, vec!["mcp_e1_echo".to_string()]);
    assert_eq!(agent.tool_names(), vec!["mcp_e1_echo".to_string()]);

    // Real dispatch through the live server (not a mock): the model calls the
    // hot-added tool and the loop's tools/call must reach the python child.
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.final_text, "done");
    assert_eq!(
        state.captured()[0].tool_names,
        vec!["mcp_e1_echo".to_string()],
        "hot-added tool reaches the request side"
    );
    let echoed = agent.messages.iter().find_map(|m| {
        m.content.iter().find_map(|b| match b {
            Block::ToolResult { content, .. } if content.starts_with("echo: ") => {
                Some(content.clone())
            }
            _ => None,
        })
    });
    assert_eq!(
        echoed.as_deref(),
        Some("echo: hi"),
        "the result must come from the real server"
    );

    // Collision rollback: adding the same server name again sanitizes to the
    // same display name → Err, and the just-spawned second child is killed by
    // dropping the fresh tool set (agent state unchanged, first server alive).
    let before = agent.tool_names();
    let err = agent.add_mcp_server(echo.clone()).unwrap_err();
    assert!(format!("{err:#}").contains("collision"));
    assert_eq!(agent.tool_names(), before);

    // Hot-remove: the last holder drops → StdioTransport::drop kills the
    // process group synchronously, so no orphan survives the call.
    assert_eq!(agent.remove_mcp_server("e1").unwrap(), 1);
    assert!(agent.tool_names().is_empty());
    if let Ok(out) = std::process::Command::new("pgrep")
        .arg("-f")
        .arg("mcp_echo.py")
        .output()
    {
        assert!(
            !out.status.success(),
            "no mcp_echo.py child may outlive remove_mcp_server (pgrep: {})",
            String::from_utf8_lossy(&out.stdout)
        );
    } // no pgrep on this platform → best-effort, skip the process check
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
                Block::ToolUse {
                    id: "a".into(),
                    name: "echo".into(),
                    input: json!({}),
                },
                Block::ToolUse {
                    id: "b".into(),
                    name: "echo".into(),
                    input: json!({}),
                },
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
            Block::ToolUse {
                id: "a".into(),
                name: "echo".into(),
                input: json!({}),
            },
            Block::ToolUse {
                id: "b".into(),
                name: "echo".into(),
                input: json!({}),
            },
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
    let (prov, state) = MockProvider::new(vec![MockTurn::Ok(ChatResponse {
        blocks: vec![
            Block::Text("partial...".into()),
            Block::ToolUse {
                id: "x".into(),
                name: "echo".into(),
                input: json!({"_raw": "trun"}),
            },
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
    assert!(
        o.tool_calls.is_empty(),
        "truncated tool_use must not execute"
    );
    assert!(!ran.load(Ordering::Relaxed), "tool.run was not called");
    assert_eq!(o.final_text, "partial..."); // partial text still surfaced
    assert_eq!(
        state.calls(),
        1,
        "truncation WITH a pending tool_use never salvages (the half-call \
         stream can't be usefully resumed)"
    );
}

// ── salvage_on_max_tokens: one-shot wrap-up on output-limit truncation ──
//
// Bench forensics: reasoning-heavy gateways burn the whole completion
// budget on thinking, leaving a truncated (or zero-block) reply and an
// empty summary. The bounded self-heal appends ONE wrap-up user turn and
// runs it like any other main turn (billed, turn-budgeted, cancel-aware).

fn max_tokens_turn(text: &str) -> MockTurn {
    MockTurn::Ok(ChatResponse {
        blocks: vec![Block::Text(text.into())],
        stop_reason: "max_tokens".into(),
        usage: None,
    })
}

#[test]
fn salvage_wraps_up_truncated_reply_once() {
    // Partial text + max_tokens → one nudge turn → the wrap-up's end_turn
    // answer becomes the run's conclusion.
    let cfg = cfg_with(5, 0, 0);
    let (prov, state) = MockProvider::new(vec![
        max_tokens_turn("partial result: the total is approximately..."),
        mock::resp_text("the total is 42", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("go")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(
        state.calls(),
        2,
        "exactly one wrap-up turn after the truncation"
    );
    assert!(
        state.captured().iter().any(|c| c
            .user_texts
            .iter()
            .any(|t| t.contains("output-token limit"))),
        "the wrap-up nudge must reach the model as a user turn"
    );
    assert_eq!(o.final_text, "the total is 42");
}

#[test]
fn salvage_disabled_by_config() {
    let mut cfg = cfg_with(5, 0, 0);
    cfg.salvage_on_max_tokens = false;
    let (prov, state) = MockProvider::new(vec![
        max_tokens_turn("partial..."),
        mock::resp_text("never served", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("go")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "max_tokens");
    assert_eq!(state.calls(), 1, "no wrap-up turn when disabled");
    assert_eq!(o.final_text, "partial...");
}

#[test]
fn salvage_fires_at_most_once() {
    // The wrap-up turn itself also truncates → returned as-is, never a
    // second nudge (bounded self-heal).
    let cfg = cfg_with(5, 0, 0);
    let (prov, state) = MockProvider::new(vec![
        max_tokens_turn("partial..."),
        max_tokens_turn("still going..."),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("go")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "max_tokens");
    assert_eq!(state.calls(), 2, "salvage runs once, never twice");
}

#[test]
fn salvage_fires_on_empty_blocks() {
    // Reasoning ate the whole budget: zero blocks + max_tokens. The empty
    // assistant message must NOT enter history (strict APIs reject it — the
    // empty-blocks break) — the nudge folds into the trailing user turn.
    let cfg = cfg_with(5, 0, 0);
    let (prov, state) = MockProvider::new(vec![
        MockTurn::Ok(ChatResponse {
            blocks: vec![],
            stop_reason: "max_tokens".into(),
            usage: None,
        }),
        mock::resp_text("answer", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("go")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(state.calls(), 2);
    assert!(
        state.captured().iter().any(|c| c
            .user_texts
            .iter()
            .any(|t| t.contains("output-token limit"))),
        "the nudge must ride a USER message (never an empty assistant one)"
    );
    assert_eq!(o.final_text, "answer");
}

#[test]
fn salvage_respects_max_turns() {
    // The wrap-up turn is an ordinary turn: with max_turns=1 there is no
    // budget left for it — the truncation surfaces honestly as
    // max_turns_reached at the gate.
    let cfg = cfg_with(1, 0, 0);
    let (prov, state) = MockProvider::new(vec![max_tokens_turn("partial...")]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("go")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "max_turns_reached");
    assert_eq!(state.calls(), 1);
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
fn stuck_loop_nudges_then_stops() {
    // 同一 (工具, 参数) 连续执行：第 3 次的 tool_result 历史块里出现纠偏提示
    // （仅历史块，records/事件保持原样），累计到第 6 次后以 stuck_loop 收尾
    // ——第 7 轮请求不得发出。
    let cfg = cfg_with(99, 0, 0);
    let mut turns: Vec<mock::MockTurn> = (0..6)
        .map(|i| mock::resp_tool(&format!("t{i}"), "echo", json!({"q": "same"}), None))
        .collect();
    turns.push(mock::resp_text("never reached", None));
    let (prov, state) = MockProvider::new(turns);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(EchoTool::new("echo"))],
        vec![user_text("loop")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "stuck_loop");
    assert_eq!(o.turns, 6);
    assert_eq!(state.calls(), 6, "第 7 轮请求不得发出");
    // nudge 只进第 3 次的 tool_result 历史块，一次即可
    let results: Vec<String> = agent
        .messages()
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            Block::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 6);
    assert!(!results[1].contains("system notice"));
    assert!(
        results[2].contains("system notice"),
        "第 3 次必须带纠偏提示"
    );
    assert!(
        !results[3].contains("system notice"),
        "提示一次即可，不得每轮重复"
    );
}

#[test]
fn stuck_streak_resets_on_changed_calls() {
    // 交替两组不同参数不算打转（只有「连续原样重复」才累计）：跑满 9 轮后
    // 正常 end_turn——护栏不得误伤健康的换法重试节奏。
    let cfg = cfg_with(99, 0, 0);
    let mut turns: Vec<mock::MockTurn> = (0..9)
        .map(|i| {
            let q = if i % 2 == 0 { "x" } else { "y" };
            mock::resp_tool(&format!("t{i}"), "echo", json!({ "q": q }), None)
        })
        .collect();
    turns.push(mock::resp_text("done", None));
    let (prov, state) = MockProvider::new(turns);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(EchoTool::new("echo"))],
        vec![user_text("loop")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(state.calls(), 10);
}

#[test]
fn stuck_detection_ignores_key_order() {
    // serde_json 开了 preserve_order：同参不同键序的裸 to_string 不同。
    // 规范化（递归排序键）后必须视为同一次调用——交替两种键序照样累计到
    // stuck_loop，否则模型换个键序就能绕过护栏。
    let cfg = cfg_with(99, 0, 0);
    let mut turns: Vec<mock::MockTurn> = (0..6)
        .map(|i| {
            let input = if i % 2 == 0 {
                json!({"a": 1, "b": 2})
            } else {
                json!({"b": 2, "a": 1})
            };
            mock::resp_tool(&format!("t{i}"), "echo", input, None)
        })
        .collect();
    turns.push(mock::resp_text("never reached", None));
    let (prov, state) = MockProvider::new(turns);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(EchoTool::new("echo"))],
        vec![user_text("loop")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "stuck_loop");
    assert_eq!(state.calls(), 6);
}

/// 状态轮询工具：参数恒定，但每次返回不同进度——「同参不同果」是有进展，
/// 不得计入打转（外审第四轮 P2 复现：10%→60% 仍被 stuck_loop 强杀）。
struct PollTool {
    n: std::sync::atomic::AtomicUsize,
}
impl Tool for PollTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "poll".into(),
            description: "test polling tool with progressing output".into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }
    fn run(&self, _input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let k = self.n.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(ToolOutput::text(format!("progress {k}0%")))
    }
}

#[test]
fn stuck_detection_treats_progressing_results_as_progress() {
    let cfg = cfg_with(99, 0, 0);
    let mut turns: Vec<mock::MockTurn> = (0..6)
        .map(|i| mock::resp_tool(&format!("t{i}"), "poll", json!({"job": 42}), None))
        .collect();
    turns.push(mock::resp_text("done", None));
    let (prov, state) = MockProvider::new(turns);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(PollTool {
            n: std::sync::atomic::AtomicUsize::new(0),
        })],
        vec![user_text("watch the job")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "end_turn", "有进展的轮询不得误判打转");
    assert_eq!(state.calls(), 7);
}

/// 每次审批都把参数改成不同页码（生效参数不同、原始参数恒定、输出恒
/// "ok"）——外审第四轮 P2 复现：按原始参数计数会误停；护栏必须键在
/// Modify 后的生效参数上。
struct ModifyPager {
    n: std::sync::atomic::AtomicUsize,
}
impl Hooks for ModifyPager {
    fn before_tool(&self, _tool: &str, _input: &Value, _ctx: &ToolCtx) -> ToolDecision {
        let k = self.n.fetch_add(1, Ordering::Relaxed) + 1;
        ToolDecision::Modify(json!({"page": k}))
    }
}

#[test]
fn stuck_detection_keys_on_modify_effective_input() {
    let cfg = cfg_with(99, 0, 0);
    let mut turns: Vec<mock::MockTurn> = (0..6)
        .map(|i| mock::resp_tool(&format!("t{i}"), "echo", json!({"page": 0}), None))
        .collect();
    turns.push(mock::resp_text("done", None));
    let (prov, state) = MockProvider::new(turns);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(EchoTool::new("echo"))],
        vec![user_text("page through")],
        test_ctx(),
    );
    agent.set_hooks(Arc::new(ModifyPager {
        n: std::sync::atomic::AtomicUsize::new(0),
    }));
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(
        o.stop_reason, "end_turn",
        "生效参数逐次不同（hook 改写）不得误判打转"
    );
    assert_eq!(state.calls(), 7);
}

#[test]
fn stuck_stop_clears_when_a_later_call_in_the_same_turn_differs() {
    // 同一轮 6 次相同调用 + 第 7 次换参：旧实现的 stuck_stop 闩锁在第 6 次
    // 置位后不再随重置清除，第 7 次的自愈救不回来（外审第四轮 P2）。轮尾
    // 直接读当前连续计数后，第 7 次把计数重置为 1 → 不停，正常进入总结轮。
    let cfg = cfg_with(99, 0, 0);
    let mut blocks: Vec<Block> = (0..6)
        .map(|i| Block::ToolUse {
            id: format!("t{i}"),
            name: "echo".into(),
            input: json!({"q": "same"}),
        })
        .collect();
    blocks.push(Block::ToolUse {
        id: "t6".into(),
        name: "echo".into(),
        input: json!({"q": "different"}),
    });
    let (prov, state) = MockProvider::new(vec![
        MockTurn::Ok(ChatResponse {
            blocks,
            stop_reason: "tool_use".into(),
            usage: None,
        }),
        mock::resp_text("summary", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(EchoTool::new("echo"))],
        vec![user_text("go")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "end_turn", "同轮换法必须解除打转停止");
    assert_eq!(state.calls(), 2);
}

/// 第 N 次执行时翻取消标志的工具——复现「打转硬停覆盖取消」（外审第四轮
/// P2）：第 6 次调用让连续计数到顶、同时用户取消落下，终止原因必须是
/// cancelled（exit 130），不是 stuck_loop（exit 1）。
struct CancelOnNth {
    n: std::sync::atomic::AtomicUsize,
    at: usize,
    flag: Arc<AtomicBool>,
}
impl Tool for CancelOnNth {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "echo".into(),
            description: "test tool tripping cancel on its Nth run".into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }
    fn run(&self, _input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        if self.n.fetch_add(1, Ordering::Relaxed) + 1 == self.at {
            self.flag.store(true, Ordering::Relaxed);
        }
        Ok(ToolOutput::text("ok"))
    }
}

#[test]
fn stuck_loop_does_not_override_a_cancel_that_landed_in_the_turn() {
    let cfg = cfg_with(99, 0, 0);
    let mut turns: Vec<mock::MockTurn> = (0..6)
        .map(|i| mock::resp_tool(&format!("t{i}"), "echo", json!({"q": "same"}), None))
        .collect();
    turns.push(mock::resp_text("never reached", None));
    let (prov, state) = MockProvider::new(turns);
    let flag = Arc::new(AtomicBool::new(false));
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(CancelOnNth {
            n: std::sync::atomic::AtomicUsize::new(0),
            at: 6,
            flag: flag.clone(),
        })],
        vec![user_text("loop")],
        test_ctx(),
    );
    agent.set_cancel(flag);
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(
        o.stop_reason, "cancelled",
        "取消必须优先于打转硬停（harness 130 映射依赖 stop_reason）"
    );
    assert_eq!(state.calls(), 6);
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
    assert_eq!(
        state.calls(),
        1,
        "no second chat call once the budget was crossed"
    );
    // The turn that crossed the cap still bills its usage honestly.
    assert_eq!(o.input_tokens, 80);
    assert_eq!(o.output_tokens, 30);
}

#[test]
fn budget_gate_cap_outranks_a_looser_limits_snapshot() {
    // Single-sourcing the fleet cap from the gate (external review: the
    // comparison used the agent's own limits snapshot): a sub-agent run
    // whose snapshot is LOOSER than the inherited gate's cap must stop at
    // the gate — today children inherit the parent's number verbatim so the
    // two can't diverge, but the first future child-scoped/clamped budget
    // would silently unbound the fleet if the snapshot won. The gate is
    // pre-armed past its cap (sibling/parent spend), so the run must refuse
    // to start turn 1 at all. run_as_subagent keeps the inherited gate
    // (a top-level run installs a fresh one from its own limits).
    let mut cfg = cfg_with(99, 0, 0);
    cfg.limits.max_total_tokens = 10_000; // snapshot far looser than the gate
    let (prov, state) = MockProvider::new(vec![mock::resp_text("done", mock::mk_usage(5, 5))]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(EchoTool::new("echo"))],
        vec![user_text("hi")],
        test_ctx(),
    );
    let gate = BudgetGate::new(100);
    gate.share(150); // fleet already past its cap
    agent.ctx.budget = Some(gate);
    let o = agent.run_as_subagent(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "budget_exceeded");
    assert_eq!(o.turns, 0);
    assert_eq!(state.calls(), 0, "refused to start turn 1 at all");
}

#[test]
fn canon_input_normalizes_numeric_spelling() {
    // External review: 1 and 1.0 serialized differently (u64 vs f64 literal)
    // used to produce DIFFERENT stuck-detection keys — a provider/gateway
    // jittering the numeric type across retries of the same call would slip
    // past the guardrail entirely. Canonicalization routes every number
    // through f64 so both spellings key identically.
    assert_eq!(
        canon_input(&json!({"n": 1})),
        canon_input(&json!({"n": 1.0})),
        "int/float spelling of the same value must key identically"
    );
    assert_eq!(
        canon_input(&json!([2, 3.0])),
        canon_input(&json!([2.0, 3])),
        "nested arrays normalize the same way"
    );
    // Distinct values still key apart (the normalization must not collapse
    // everything into one bucket).
    assert_ne!(canon_input(&json!({"n": 1})), canon_input(&json!({"n": 2})));
    assert_ne!(
        canon_input(&json!({"n": 1})),
        canon_input(&json!({"n": 1.5}))
    );
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
    assert_eq!(
        counter.used.load(Ordering::Relaxed),
        65,
        "both chats billed: 40 + 25"
    );
    // Second run: fresh provider script, fresh spend — the counter must
    // NOT still carry the previous run's 65.
    let (prov2, _) = MockProvider::new(vec![mock::resp_text("again", mock::mk_usage(7, 3))]);
    agent.provider = std::sync::Arc::new(prov2);
    agent.run(&mut |_| {}).unwrap();
    let counter2 = agent.ctx.budget.clone().expect("counter re-installed");
    assert_eq!(
        counter2.used.load(Ordering::Relaxed),
        10,
        "fresh counter per run"
    );
    assert!(
        !(std::sync::Arc::ptr_eq(&counter.used, &counter2.used)),
        "a new run must replace, not reuse, the counter"
    );
}

#[test]
fn tool_billed_usage_does_not_double_count_in_the_fleet_counter() {
    // The tool-billed fold splits on ToolOutput::usage_in_shared_budget.
    // `true` (the `task` tool's case: the child billed its own chats into the
    // inherited counter while it ran) folds into the parent's TOTALS without
    // re-sharing — a double add would halve the fleet's effective budget.
    let mut cfg = cfg_with(99, 0, 0);
    cfg.limits.max_total_tokens = 1_000;
    let (prov, _) = MockProvider::new(vec![
        mock::resp_tool("a", "task", json!({"label": "x"}), mock::mk_usage(40, 0)),
        mock::resp_text("done", mock::mk_usage(10, 0)),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(TaskTool)],
        vec![user_text("hi")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    // Accounting total = both chats (40+10) + the child's billed input 100.
    assert_eq!(o.input_tokens, 150);
    // Fleet counter = direct chats ONLY (40+10); TaskTool's 110 models a
    // child that already shared its spend into the counter itself.
    let counter = agent.ctx.budget.clone().expect("counter installed");
    assert_eq!(counter.used.load(Ordering::Relaxed), 50);
}

#[test]
fn leaf_tool_billed_usage_shares_into_the_fleet_counter() {
    // `usage_in_shared_budget=false` (the default: a host CallbackTool or any
    // non-subagent tool reporting usage) — that spend entered no counter
    // anywhere, so the agent's fold shares it. It used to bill the totals yet
    // stay invisible to the max_total_tokens gate (external review P2-3).
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
    // Totals unchanged by the flag: both chats' input (40+10) + the tool's
    // billed 111 input / 22 output.
    assert_eq!(o.input_tokens, 161);
    assert_eq!(o.output_tokens, 22);
    // Fleet counter now sees the leaf tool's spend too: 40 + 10 + 111 + 22.
    let counter = agent.ctx.budget.clone().expect("counter installed");
    assert_eq!(
        counter.used.load(Ordering::Relaxed),
        183,
        "leaf-reported usage must count against the budget gate"
    );
}

#[test]
fn leaf_tool_usage_trips_the_budget_gate() {
    // The external-review P2-3 repro: max_total_tokens=5, turn 1's chat costs
    // 2 and the custom tool reports 100 — the tool spend MUST be visible to
    // the turn-boundary gate, stopping the run with budget_exceeded instead
    // of starting turn 2 (previously the gate saw only the 2).
    let mut cfg = cfg_with(99, 0, 0);
    cfg.limits.max_total_tokens = 5;
    let (prov, state) = MockProvider::new(vec![
        mock::resp_tool("a", "bill", json!({}), mock::mk_usage(2, 0)),
        mock::resp_text("done", None), // turn 2, must never run
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(LeafHundredBillTool)],
        vec![user_text("hi")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "budget_exceeded");
    assert_eq!(o.turns, 1);
    assert_eq!(state.calls(), 1, "no second chat call past the cap");
    // Honest billing either way: 2 (chat) + 100 (tool) folded into totals.
    assert_eq!(o.input_tokens, 102);
}

/// BillTool variant for the gate repro: reports exactly 100 input tokens.
struct LeafHundredBillTool;
impl Tool for LeafHundredBillTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "bill".into(),
            description: "reports 100 tokens".into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }
    fn run(&self, _input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        Ok(ToolOutput {
            content: "billed".into(),
            image: None,
            is_error: false,
            usage: Some(crate::provider::Usage {
                input_tokens: 100,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            }),
            usage_in_shared_budget: false,
            meta: None,
        })
    }
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
fn request_error_past_deadline_ends_run_as_timeout() {
    // External re-review P2: timeout_secs=1 and the in-flight request dies
    // once the budget is gone (its per-request timeout WAS the deadline
    // remainder) — the run must end as a clean "timeout" (exit 124
    // downstream, transcript intact), not a generic provider error that
    // climbs the retry ladder.
    let cfg = cfg_with(5, 1, 0);
    let (prov, _) = MockProvider::new(vec![MockTurn::ErrAfter {
        delay: std::time::Duration::from_millis(1200),
        error: anyhow::anyhow!("request to API failed: operation timed out"),
    }]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "timeout");
    assert_eq!(o.turns, 1);
}

#[test]
fn api_status_error_retry_taxonomy() {
    // The RunError classification both retry drivers share: deterministic
    // 4xx rejections are NOT transient (no backoff ladder), 429/5xx and
    // unrecognized errors stay transient (the pre-taxonomy default).
    let classify = |status: u16| {
        RunError {
            source: crate::provider::ApiStatusError::new(
                status,
                format!("X API error {status}: nope"),
            )
            .into(),
            usage: Usage::default(),
            tool_calls: vec![],
            compressions: vec![],
            turns: 1,
            elapsed: std::time::Duration::ZERO,
        }
        .is_transient()
    };
    for s in [400, 401, 403, 404, 422] {
        assert!(!classify(s), "{s} must be deterministic");
    }
    for s in [429, 500, 502, 503] {
        assert!(classify(s), "{s} must stay transient");
    }
    // The classification sees the structured status THROUGH anyhow context
    // wrapping (the providers' .context("request … failed") send paths).
    let api_err: anyhow::Error =
        crate::provider::ApiStatusError::new(401, "X API error 401: nope".into()).into();
    let wrapped = RunError {
        // anyhow::Error's inherent .context — no trait import needed.
        source: api_err.context("request to API failed"),
        usage: Usage::default(),
        tool_calls: vec![],
        compressions: vec![],
        turns: 1,
        elapsed: std::time::Duration::ZERO,
    };
    assert!(!wrapped.is_transient(), "chain walk must find the status");
    // Unrecognized errors (network blips from custom providers, etc.) keep
    // the pre-taxonomy retryable default.
    let plain = RunError {
        source: anyhow::anyhow!("connection reset by peer"),
        usage: Usage::default(),
        tool_calls: vec![],
        compressions: vec![],
        turns: 1,
        elapsed: std::time::Duration::ZERO,
    };
    assert!(plain.is_transient());
}

#[test]
fn is_transient_treats_408_request_timeout_and_425_too_early_as_retryable() {
    // External re-review P2: 408 (Request Timeout) and 425 (Too Early) are
    // the server EXPLICITLY saying "send this again" — 408 after the client
    // stalled too long, 425 before the retry-after window elapsed. The old
    // `429 || >=500` set dropped them into the deterministic lane, so one
    // 408 blip killed the run without ever touching the backoff ladder.
    // They must ride the ladder like 429 does.
    let classify = |status: u16| {
        RunError {
            source: crate::provider::ApiStatusError::new(
                status,
                format!("X API error {status}: retry me"),
            )
            .into(),
            usage: Usage::default(),
            tool_calls: vec![],
            compressions: vec![],
            turns: 1,
            elapsed: std::time::Duration::ZERO,
        }
        .is_transient()
    };
    assert!(classify(408), "408 Request Timeout is transient");
    assert!(classify(425), "425 Too Early is transient");
}

#[test]
fn timeout_zero_means_no_timeout() {
    // Guards the elapsed()>=0 trap: with timeout_secs=0 the run must
    // proceed normally instead of timing out before its first turn.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![mock::resp_text("ok", None)]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
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
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "timeout");
    assert_eq!(o.turns, 1);
    assert_eq!(
        state.calls(),
        1,
        "no request started on a sub-second budget"
    );
}

#[test]
fn timeout_one_second_still_runs_first_turn() {
    // The sub-second floor is gated on turns>0: with timeout_secs=1 the
    // FIRST turn starts with (nearly) the full budget and must run to
    // completion, not be refused up front.
    let cfg = cfg_with(5, 1, 0);
    let (prov, state) = MockProvider::new(vec![mock::resp_text("ok", None)]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
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
fn all_zero_usage_object_emits_no_event_but_nonzero_still_does() {
    // Some gateways/custom providers return an EXPLICIT usage object with
    // every axis zero (frame present, nothing billed) instead of omitting
    // it. The None side is already pinned (compress_without_provider_usage_
    // emits_no_zero_event); this pins the Some(all-zero) side: no Usage
    // {0,0,0,0} event noise, totals untouched — while a genuinely non-zero
    // report on the very next call still folds and emits as before.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![
        // Turn 1: tool call whose chat reports Some(all-zero) usage.
        mock::resp_tool("a", "echo", json!({}), Some(Usage::default())),
        // Turn 2: real usage — must NOT be silenced by the zero short-circuit.
        mock::resp_text("done", mock::mk_usage(100, 10)),
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
    // Totals: only turn 2's real report — zeros contributed nothing.
    assert_eq!(o.input_tokens, 100);
    assert_eq!(o.output_tokens, 10);
    // Exactly ONE Usage event (turn 2's); the zero object emitted none.
    let usage_events = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Usage { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        usage_events.len(),
        1,
        "Some(all-zero) usage must not emit a Usage event; nonzero must"
    );
    assert!(matches!(
        usage_events[0],
        AgentEvent::Usage {
            input_tokens: 100,
            output_tokens: 10,
            ..
        }
    ));
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
        mock::resp_tool(
            "a",
            "echo",
            json!({}),
            mock::mk_usage_cache(100, 10, 300, 1500),
        ),
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
        .filter(|e| matches!(e, AgentEvent::Usage { .. }))
        .count();
    assert_eq!(cache_events, 2);
}

#[test]
fn compress_folds_history_and_emits_event() {
    // History over threshold + a safe cut → summarize runs (an extra chat
    // call), the middle folds into message[0], a Compressed event fires,
    // and the main request sees the shorter folded history. The middle
    // assistant message is big enough that the fold really shrinks the
    // estimated context — the head is retained verbatim and middle user
    // texts are quoted back, so only the dropped assistant bulk is freed
    // against the fixed summary scaffold (a tiny fixture would show a
    // larger estimate AFTER the fold, with no honest before>after signal).
    let mut cfg = cfg_with(5, 0, 30);
    cfg.limits.keep_recent_messages = 2;
    let long = "x".repeat(40);
    let bulk = "y".repeat(2000);
    // 5 messages → n > keep_recent+2; cut candidate 3 = assistant → Some(3)
    let initial = vec![
        user_text(&long),
        Message::assistant(vec![Block::Text(bulk)]),
        user_text("u1"),
        Message::assistant(vec![Block::Text("a2".into())]),
        user_text("u2"),
    ];
    let (prov, state) = MockProvider::new(vec![
        mock::resp_text("SUMMARY", None),      // summarize_prefix call
        mock::resp_text("final answer", None), // main call
    ]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(state.calls(), 2);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Compressed {
                dropped: 2,
                kept: 3,
                trigger: CompressTrigger::Preflight,
                ..
            }
        )),
        "compress should drop 2 middle messages, keep 3, tagged preflight"
    );
    // The main (2nd) request saw the folded history: 3 messages.
    let cap = state.captured();
    assert_eq!(cap[1].message_count, 3);
    // Observability pair (A2): this compress ran on turn 1 — before ANY
    // provider-reported usage — so there is no valid anchor and
    // `measured_tokens_before` must stay None (an estimate must never be
    // passed off as a measurement). The estimate pair still moved: the fold
    // shrank the context.
    for e in &events {
        if let AgentEvent::Compressed {
            estimated_tokens_before,
            estimated_tokens_after,
            measured_tokens_before,
            ..
        } = e
        {
            assert!(
                estimated_tokens_before > estimated_tokens_after,
                "fold must shrink the estimate: {estimated_tokens_before} -> {estimated_tokens_after}"
            );
            assert_eq!(
                *measured_tokens_before, None,
                "turn-1 compress has no anchor: measured must be None"
            );
        }
    }
    // ...and the run-level audit record mirrors the event exactly.
    assert_eq!(o.compressions.len(), 1);
    let c = &o.compressions[0];
    assert_eq!(c.trigger, "preflight");
    assert_eq!((c.dropped, c.kept), (2, 3));
    assert_eq!(c.measured_tokens_before, None);
}

#[test]
fn compress_records_anchor_backed_measured_tokens() {
    // (A) A compress that fires on turn 3 — after two usage-bearing main
    // calls — reports the provider-MEASURED prompt tokens of the live anchor
    // alongside the estimates. The estimate pair still brackets the fold
    // (before > after), and the run-level CompressionRecord matches the
    // event field for field.
    let mut cfg = cfg_with(5, 0, 30);
    cfg.limits.keep_recent_messages = 2;
    let t1 = "x".repeat(40);
    let t2 = "y".repeat(40);
    // Turn 1/2: big assistant text + a tool call, each with usage (so the
    // anchor is live and current when turn 3's preflight compress runs).
    // History: turn-2 preflight trips the 30-char gate but finds no cut
    // (3 messages <= keep+2); turn-3 preflight trips again with 5 messages.
    let (prov, _) = MockProvider::new(vec![
        MockTurn::Ok(ChatResponse {
            blocks: vec![
                Block::Text(t1),
                Block::ToolUse {
                    id: "a".into(),
                    name: "echo".into(),
                    input: json!({}),
                },
            ],
            stop_reason: "tool_use".into(),
            usage: mock::mk_usage(50, 5),
        }),
        MockTurn::Ok(ChatResponse {
            blocks: vec![
                Block::Text(t2),
                Block::ToolUse {
                    id: "b".into(),
                    name: "echo".into(),
                    input: json!({}),
                },
            ],
            stop_reason: "tool_use".into(),
            usage: mock::mk_usage(60, 5),
        }),
        mock::resp_text("SUMMARY", None), // summarize_prefix call
        mock::resp_text("final answer", None), // main call
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(EchoTool::new("echo"))],
        vec![user_text("hi")],
        test_ctx(),
    );
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    let event_vals = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Compressed {
                dropped,
                kept,
                trigger,
                estimated_tokens_before,
                estimated_tokens_after,
                measured_tokens_before,
            } => Some((
                *dropped,
                *kept,
                *trigger,
                *estimated_tokens_before,
                *estimated_tokens_after,
                *measured_tokens_before,
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(event_vals.len(), 1, "exactly one compress event");
    let (dropped, kept, trigger, est_before, est_after, measured) = event_vals[0];
    assert_eq!((dropped, kept), (2, 3));
    assert!(matches!(trigger, CompressTrigger::Preflight));
    assert!(
        est_before > est_after,
        "fold shrinks the estimate: {est_before} -> {est_after}"
    );
    // The anchor from turn 2's response (60↑) is live at the fold — the
    // provider-measured prompt tokens ride along honestly.
    assert_eq!(
        measured,
        Some(60),
        "measured = the live anchor's prompt_tokens"
    );
    // Run-level audit record: same story, stringified trigger.
    assert_eq!(o.compressions.len(), 1);
    let c = &o.compressions[0];
    assert_eq!(c.trigger, "preflight");
    assert_eq!((c.dropped, c.kept), (2, 3));
    assert_eq!(c.estimated_tokens_before, est_before);
    assert_eq!(c.estimated_tokens_after, est_after);
    assert_eq!(c.measured_tokens_before, Some(60));
}

#[test]
fn compress_reports_none_measured_after_anchor_voiding_prune() {
    // (C) The third anchor state, between "never had one" (None) and "live"
    // (Some): the anchor was MINTED (turn 1 reported usage), then the
    // preflight prune rewrote history and voided it (try_prune_protect:
    // "History rewritten ⇒ the usage anchor … is void") — but freed too
    // little for the gate to clear, so the fold still ran. The Compressed
    // event / audit record must carry measured=None: a provider measurement
    // of a history that no longer exists must never be passed off as the
    // new history's size (诚实边界).
    //
    // Geometry: turn 2 reports NO usage, so the anchor stays turn-1's
    // (msg_count 1) — its prefix is ONLY message[0] ("hi"), which the prune
    // never touches (original-task head + keep_recent tail are protected).
    // All four liveness conditions would therefore still hold after the
    // prune; ONLY the explicit `usage_anchor = None` in try_prune_protect
    // keeps `measured` honest here — remove it and this test sees the
    // stale turn-1 measurement (Some(50)) instead of None.
    let mut cfg = cfg_with(5, 0, 30);
    cfg.limits.keep_recent_messages = 2;
    let t1 = "x".repeat(400);
    let pad = "z".repeat(30_000);
    let (prov, state) = MockProvider::new(vec![
        // Turn 1: usage (mints the anchor) + a tool call whose 30k-char
        // echo result is the prune fuel (history: hi / t1+tool_use a /
        // tool_result a BIG).
        MockTurn::Ok(ChatResponse {
            blocks: vec![
                Block::Text(t1),
                Block::ToolUse {
                    id: "a".into(),
                    name: "bulk".into(),
                    input: json!({"pad": pad}),
                },
            ],
            stop_reason: "tool_use".into(),
            usage: mock::mk_usage(50, 5),
        }),
        // Turn 2: a plain tool call, no usage — the anchor must stay stale
        // (msg_count 1) so the prune's void is observable. History grows to
        // 5 messages; the BIG result is now outside the protected window.
        mock::resp_tool("b", "echo", json!({}), None),
        // Turn 3 preflight: prune frees ~30k (anchor voided) but the 30-char
        // gate still trips → summarize_prefix call → the fold.
        mock::resp_text("SUMMARY", None),
        mock::resp_text("final answer", None), // main call on the folded history
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![
            Box::new(EchoTool::new("echo")),
            Box::new(InputEchoTool { name: "bulk" }),
        ],
        vec![user_text("hi")],
        test_ctx(),
    );
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(state.calls(), 4);
    // The main request ran on the folded history (3 messages).
    let cap = state.captured();
    assert_eq!(cap[3].message_count, 3);
    // Precondition, observable side: turn 1's usage minted the anchor —
    // the None below is only meaningful because a measurement existed.
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Usage { .. })),
        "turn-1 usage must mint the anchor before the prune voids it"
    );
    // The voiding path really ran, in order: prune (history rewritten ⇒
    // anchor voided) strictly before the fold.
    let pruned_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Pruned { .. }))
        .expect("prune must fire before the compress");
    let compressed_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Compressed { .. }))
        .expect("fold must still run: the prune freed too little to clear the gate");
    assert!(
        pruned_idx < compressed_idx,
        "anchor-voiding prune must precede the compress"
    );
    for e in &events {
        if let AgentEvent::Compressed {
            trigger,
            dropped,
            kept,
            estimated_tokens_before,
            estimated_tokens_after,
            measured_tokens_before,
        } = e
        {
            assert!(matches!(trigger, CompressTrigger::Preflight));
            assert_eq!((*dropped, *kept), (2, 3));
            assert!(
                estimated_tokens_before > estimated_tokens_after,
                "fold must shrink the estimate: {estimated_tokens_before} -> {estimated_tokens_after}"
            );
            assert_eq!(
                *measured_tokens_before, None,
                "anchor voided by the prune: measured must be None, not the stale 50"
            );
        }
    }
    // Run-level audit record mirrors the event.
    assert_eq!(o.compressions.len(), 1);
    let c = &o.compressions[0];
    assert_eq!(c.trigger, "preflight");
    assert_eq!((c.dropped, c.kept), (2, 3));
    assert_eq!(c.measured_tokens_before, None);
}

#[test]
fn failed_run_error_still_carries_compressions() {
    // (B) 错误路径也是账单面: the run compressed its history, then the main
    // call died — the RunError must carry the compression audit so the
    // harness's failure-path result.json can report the fold that happened
    // before the death. The prefix is big enough for a real fold (see the
    // compress_folds test — the estimate must shrink through it).
    let mut cfg = cfg_with(5, 0, 30);
    cfg.limits.keep_recent_messages = 2;
    let long = "x".repeat(40);
    let bulk = "y".repeat(2000);
    let initial = vec![
        user_text(&long),
        Message::assistant(vec![Block::Text(bulk)]),
        user_text("u1"),
        Message::assistant(vec![Block::Text("a2".into())]),
        user_text("u2"),
    ];
    let (prov, _) = MockProvider::new(vec![
        mock::resp_text("SUMMARY", None), // summarize_prefix call
        MockTurn::Err("provider blew up".into()),
    ]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let mut events = Vec::new();
    // Match by hand: RunOutcome isn't Debug, so unwrap_err() won't compile.
    let err = match agent.run(&mut |e| events.push(e)) {
        Err(e) => e,
        Ok(_) => panic!("main-call failure must propagate as RunError"),
    };
    assert_eq!(err.compressions.len(), 1, "audit survives the failure");
    let c = &err.compressions[0];
    assert_eq!(c.trigger, "preflight");
    assert_eq!((c.dropped, c.kept), (2, 3));
    assert!(
        c.estimated_tokens_before > c.estimated_tokens_after,
        "fold shrank the estimate before the run died"
    );
}

#[test]
fn turn_timings_count_turns_and_tool_calls() {
    // (C) One tool turn + one text turn: rows per turn, len == turns, and
    // each row's tool_calls is THAT turn's records delta (1, then 0).
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![
        mock::resp_tool("t", "echo", json!({}), None),
        mock::resp_text("final", None),
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
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(o.turns, 2);
    assert_eq!(o.turn_timings.len(), 2, "one row per turn, len == turns");
    assert_eq!(o.turn_timings[0].turn, 1);
    assert_eq!(o.turn_timings[0].tool_calls, 1);
    assert_eq!(o.turn_timings[1].turn, 2);
    assert_eq!(o.turn_timings[1].tool_calls, 0);
}

#[test]
fn tool_call_records_carry_execution_duration() {
    // (D1) A real execution's record carries its wall-clock duration —
    // inline-Sequential (60ms) and parallel-worker (2×60ms overlapping) both.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![
        MockTurn::Ok(ChatResponse {
            blocks: vec![
                Block::ToolUse {
                    id: "a".into(),
                    name: "slow".into(),
                    input: json!({"label": "go"}),
                },
                Block::ToolUse {
                    id: "b".into(),
                    name: "pslow".into(),
                    input: json!({"label": "go"}),
                },
                Block::ToolUse {
                    id: "c".into(),
                    name: "pslow".into(),
                    input: json!({"label": "go"}),
                },
            ],
            stop_reason: "tool_use".into(),
            usage: None,
        }),
        mock::resp_text("done", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![
            Box::new(SlowTool::custom("slow", 60, ToolConcurrency::Sequential)),
            Box::new(SlowTool::custom("pslow", 60, ToolConcurrency::Parallel)),
        ],
        vec![user_text("go")],
        test_ctx(),
    );
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(o.tool_calls.len(), 3);
    // All three really ran: durations present and at least ~2/3 of the sleep
    // (wall-clock jitter slack — the assertion is a floor, not a pin).
    for rec in &o.tool_calls {
        let d = rec
            .duration_ms
            .expect("executed tool carries Some(duration_ms)");
        assert!(d >= 40, "60ms sleep must read >= 40ms, got {d}");
    }
    // The two parallel workers' 60ms sleeps overlap: their total wall time
    // is far below the serialized 3×60ms (existing timing tests pin the
    // parallelism itself; here it just sanity-checks the durations).
    assert_eq!(o.turn_timings.len(), 2, "one row per turn");
    assert_eq!(o.turn_timings[0].turn, 1);
    assert_eq!(
        o.turn_timings[0].tool_calls, 3,
        "all three calls recorded in turn 1"
    );
    assert_eq!(o.turn_timings[1].tool_calls, 0);
}

#[test]
fn cancelled_call_records_no_duration() {
    // (D2) A call that passed its gate but was stopped by the gate→execution
    // cancel checkpoint never executed: its record reports duration_ms None
    // (and the error output), while the earlier real execution keeps its
    // measured duration. Deterministic: the hook flips cancel during the
    // SECOND call's gate, so the checkpoint (not a race) stops it.
    struct CancelOnGate;
    impl Hooks for CancelOnGate {
        fn before_tool(&self, tool: &str, _input: &Value, ctx: &ToolCtx) -> ToolDecision {
            if tool == "echo" {
                if let Some(c) = &ctx.cancel {
                    c.store(true, Ordering::Relaxed);
                }
            }
            ToolDecision::Allow
        }
    }
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![MockTurn::Ok(ChatResponse {
        blocks: vec![
            Block::ToolUse {
                id: "a".into(),
                name: "slow".into(),
                input: json!({"label": "go"}),
            },
            Block::ToolUse {
                id: "b".into(),
                name: "echo".into(),
                input: json!({}),
            },
        ],
        stop_reason: "tool_use".into(),
        usage: None,
    })]);
    let flag = Arc::new(AtomicBool::new(false));
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![
            Box::new(SlowTool::custom("slow", 60, ToolConcurrency::Sequential)),
            Box::new(EchoTool::new("echo")),
        ],
        vec![user_text("go")],
        test_ctx(),
    );
    agent.set_hooks(Arc::new(CancelOnGate));
    agent.set_cancel(flag);
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "cancelled");
    assert_eq!(o.tool_calls.len(), 2);
    // The slow tool really ran.
    let d = o.tool_calls[0]
        .duration_ms
        .expect("executed tool carries Some(duration_ms)");
    assert!(d >= 40, "60ms sleep must read >= 40ms, got {d}");
    // The echo call never executed: no duration, error output.
    assert_eq!(o.tool_calls[1].duration_ms, None);
    assert!(o.tool_calls[1].is_error);
    assert_eq!(o.tool_calls[1].output, "[cancelled by user]");
    // History still answers both tool_use 1:1.
    assert_eq!(
        agent.trailing_tool_result_contents(),
        vec!["slow-go".to_string(), "[cancelled by user]".to_string()]
    );
    // The cancelled turn still got its timing row (the row lands at the
    // turn body's natural end, before the cancel check breaks the loop).
    assert_eq!(o.turn_timings.len(), 1);
    assert_eq!(o.turn_timings[0].tool_calls, 2);
}

#[test]
fn failed_compress_still_bills_its_burned_summary_tokens() {
    // Honest cost (#3, 2026-09 review): a summary request that dies mid-call
    // (here: a non-end_turn stop_reason with usage already reported) still
    // BILLED those tokens. They used to vanish — the bail dropped `spent`,
    // and neither the run totals nor result.json ever saw them. The
    // CompressError plumbing must fold them into the run via bill_chat.
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
        // The summary call: partial text + usage reported, then a
        // non-end_turn stop (the mid-stream death shape).
        MockTurn::Ok(ChatResponse {
            blocks: vec![Block::Text("half-written su".into())],
            stop_reason: "refusal".into(),
            usage: mock::mk_usage(77, 7),
        }),
        mock::resp_text("final answer", None), // main call, no usage
    ]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(state.calls(), 2, "summary attempt + main call");
    // The burned tokens landed in the run totals (the main call billed 0).
    assert_eq!(o.input_tokens, 77, "burned summary input tokens billed");
    assert_eq!(o.output_tokens, 7, "burned summary output tokens billed");
    // ...and the failure was still surfaced (compress disabled for the run).
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Warning(w) if w.contains("context compression failed")
        )),
        "compress failure warns: {events:?}"
    );
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
            AgentEvent::Compressed {
                dropped: 4,
                kept: 1,
                trigger: CompressTrigger::Preflight,
                ..
            }
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
        mock::resp_text("SUMMARY", None), // forced compression call
        mock::resp_text("final answer", None), // retried main call
    ]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(state.calls(), 3);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Compressed {
                trigger: CompressTrigger::Overflow,
                ..
            }
        )),
        "overflow should trigger a forced compression"
    );
    // The retried main request saw the folded 1-message history.
    let cap = state.captured();
    assert_eq!(cap[2].message_count, 1);
}

#[test]
fn preflight_prune_can_replace_the_summary_call() {
    // A history whose bulk is one stale 30k tool result: pruning alone gets
    // it back under the char threshold, so the run must NOT bill a summary
    // request — the cheap gate fires, the placeholder lands, the single main
    // call proceeds on the pruned history.
    let mut cfg = cfg_with(5, 0, 200);
    cfg.limits.keep_recent_messages = 2;
    let mut initial = vec![user_text("task")];
    initial.extend(big_tool_turn("t1", "shell", 30_000));
    initial.push(user_text("u1"));
    initial.push(Message::assistant(vec![Block::Text("a2".into())]));
    initial.push(user_text("u2"));
    let (prov, state) = MockProvider::new(vec![mock::resp_text("final answer", None)]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(
        state.calls(),
        1,
        "no summary call — the prune alone resolved the pressure"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Pruned {
            messages: 1,
            chars_freed
        } if *chars_freed >= PRUNE_MINIMUM_CHARS
    )));
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::Compressed { .. })));
    // The placeholder is what the (only) request carried, and it persists.
    let Block::ToolResult { content, .. } = &agent.messages()[2].content[0] else {
        panic!("tool result block");
    };
    assert!(content.starts_with("[pruned tool output"));
    // Pruning replaces blocks in place — the N:N pairing invariant holds on
    // the request the provider actually saw.
    let cap = state.captured();
    assert_eq!(cap[0].tool_use_ids, cap[0].tool_result_ids);
}

#[test]
fn preflight_prune_insufficient_still_summarizes() {
    // Prune fired (≥ minimum freed) but the threshold is STILL exceeded →
    // the LLM summary path runs on top: both events, summary + main calls,
    // and the summarizer's transcript contains the placeholder, not the
    // 30k dump it no longer needs to re-read.
    let mut cfg = cfg_with(5, 0, 30);
    cfg.limits.keep_recent_messages = 2;
    let mut initial = vec![user_text("task")];
    initial.extend(big_tool_turn("t1", "shell", 30_000));
    initial.push(user_text("u1"));
    initial.push(Message::assistant(vec![Block::Text("a2".into())]));
    initial.push(user_text("u2"));
    let (prov, state) = MockProvider::new(vec![
        mock::resp_text("SUMMARY", None),      // summarize_prefix call
        mock::resp_text("final answer", None), // main call on folded history
    ]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(state.calls(), 2);
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Pruned { .. })));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Compressed {
            trigger: CompressTrigger::Preflight,
            ..
        }
    )));
}

#[test]
fn overflow_recovery_prefers_prune_over_summary() {
    // No threshold configured (the provider is the only oracle): the first
    // overflow prunes the stale 30k result and retries WITHOUT a summary
    // call; the retried request succeeds on the placeholder history.
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.keep_recent_messages = 2;
    let mut initial = vec![user_text("task")];
    initial.extend(big_tool_turn("t1", "shell", 30_000));
    initial.push(user_text("u1"));
    initial.push(Message::assistant(vec![Block::Text("a2".into())]));
    initial.push(user_text("u2"));
    let (prov, state) = MockProvider::new(vec![
        MockTurn::Err("mock: maximum context length exceeded".into()),
        mock::resp_text("final answer", None), // retried main call
    ]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(state.calls(), 2, "prune + retry only — no summary call");
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Pruned { .. })));
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::Compressed { .. })));
    // The retried request still carried the whole history (prune replaces
    // blocks in place; normalize_adjacent merged the two adjacent user
    // messages 6→5 as always) with the N:N pairing intact.
    let cap = state.captured();
    assert_eq!(cap[1].message_count, 5);
    assert_eq!(cap[1].tool_use_ids, cap[1].tool_result_ids);
}

#[test]
fn summary_call_retries_one_transient_failure() {
    // One lost packet on the summary request must NOT disable compression
    // for the whole run (compress_failed gates every later fold AND the
    // overflow recovery): after a ~1s backoff the summary retried once and
    // the fold proceeds. Without the retry the mock would run out of script
    // (no SUMMARY, no fold, no Compressed event).
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
        MockTurn::Err("mock: connection reset by peer".into()), // transient blip
        mock::resp_text("SUMMARY", None),                       // retried summary
        mock::resp_text("final answer", None),                  // main call
    ]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(state.calls(), 3, "blip + retried summary + main");
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Compressed {
            trigger: CompressTrigger::Preflight,
            ..
        }
    )));
}

/// A provider whose FIRST summary-shaped call (`cache:false` — the one-off
/// summary requests opt out of caching, main turns don't; see the mock
/// `Captured::cache` doc) fails transiently and flips the run's shared cancel
/// flag shortly AFTER that failure returns — landing inside the summary
/// retry's backoff sleep. The delay is the point: setting the flag before
/// chat() returns would trip the retry arm's own `!cancelled()` gate (which
/// correctly declines to retry), never reaching the sleep whose return value
/// is the defect under test. Later calls succeed, so the buggy behavior (a
/// second summary upload) shows up as a count, not a script-exhausted panic.
struct TransientFailThenCancelProvider {
    flag: Arc<AtomicBool>,
    summary_calls: Arc<std::sync::atomic::AtomicUsize>,
}
impl Provider for TransientFailThenCancelProvider {
    fn chat(
        &self,
        req: &ChatRequest,
        _on_event: &mut dyn FnMut(ChatEvent),
    ) -> Result<ChatResponse> {
        if !req.cache {
            self.summary_calls.fetch_add(1, Ordering::SeqCst);
            if self.summary_calls.load(Ordering::SeqCst) == 1 {
                // backoff(0) sleeps 750–1250ms in 100ms slices; the store at
                // +50ms is seen by a slice check well inside that window and
                // comfortably after the pre-sleep cancelled() gate ran clear.
                let flag = self.flag.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(50));
                    flag.store(true, Ordering::SeqCst);
                });
                return Err(anyhow::anyhow!("mock: connection reset by peer"));
            }
        }
        Ok(ChatResponse {
            blocks: vec![Block::Text("unused main reply".into())],
            stop_reason: "end_turn".into(),
            usage: None,
        })
    }
}

#[test]
fn summary_retry_backoff_aborts_instead_of_resending_when_cancelled_mid_sleep() {
    // The summary retry's backoff used to discard sleep_cancelable's return:
    // a cancel landing DURING the sleep was ignored and the (possibly
    // multi-MB) summary prompt was re-uploaded on a run the user had already
    // aborted — only the retried request's own SSE cancel check stopped it,
    // after the body had left. The retry must instead bail with the cancel
    // error shape, so the run ends cancelled with exactly ONE summary request
    // ever sent.
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
    let flag = Arc::new(AtomicBool::new(false));
    let summary_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let prov = TransientFailThenCancelProvider {
        flag: flag.clone(),
        summary_calls: summary_calls.clone(),
    };
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    agent.set_cancel(flag);
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "cancelled");
    // The load-bearing assertion: no second summary request went out. On the
    // buggy path this counts 2 (the blip + the resent prompt the user never
    // asked for); on the fixed path the backoff's cancel check returned early.
    assert_eq!(
        summary_calls.load(Ordering::SeqCst),
        1,
        "a cancelled run must not re-upload the summary prompt"
    );
    // The bail surfaced through the documented channel — the compress-failure
    // warning carrying the cancel error — and no fold ever completed.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Warning(w) if w.contains("cancelled during retry backoff")
    )));
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::Compressed { .. })));
}

#[test]
fn overflow_backstop_prunes_the_protected_tail() {
    // Small-window backstop: after the LLM fold's retry STILL overflows, the
    // configured protection window guards exactly the tail that's too big.
    // A descending window (half → floor 1) prunes the tail's oversized tool
    // result to a placeholder (original archived) and retries once more.
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.keep_recent_messages = 4;
    let mut initial = vec![user_text("task")];
    initial.push(Message::assistant(vec![Block::Text("a1".into())]));
    initial.push(user_text("u1"));
    initial.extend(big_tool_turn("t1", "shell", 30_000)); // tail pair
    initial.push(Message::assistant(vec![Block::Text("a2".into())]));
    initial.push(user_text("u2"));
    let (prov, state) = MockProvider::new(vec![
        MockTurn::Err("mock: maximum context length exceeded".into()), // req 1
        mock::resp_text("SUMMARY", None),                              // forced compression call
        MockTurn::Err("mock: maximum context length exceeded".into()), // retry still over
        mock::resp_text("final answer", None),                         // backstop retry
    ]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(state.calls(), 4);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Compressed {
            trigger: CompressTrigger::Overflow,
            ..
        }
    )));
    // The backstop's prune fired on the protected tail (the first-aid pass
    // at the configured window had nothing: the tail IS the window).
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Pruned {
            chars_freed,
            ..
        } if *chars_freed >= PRUNE_MINIMUM_CHARS
    )));
    // The live history now carries the placeholder; pairing intact.
    let cap = state.captured();
    assert_eq!(cap[3].tool_use_ids, cap[3].tool_result_ids);
    let pruned = agent
        .messages()
        .iter()
        .flat_map(|m| m.content.iter())
        .any(|b| matches!(b, Block::ToolResult { content, .. } if content.starts_with("[pruned tool output")));
    assert!(pruned, "the tail's oversized result became a placeholder");
}

#[test]
fn preflight_estimates_from_provider_reported_usage() {
    // The heuristic is model-agnostic: on a Chinese-heavy history its CJK
    // weighting is close, but the provider's own tokenizer can still disagree.
    // After one real request the anchor (the provider's own prompt-token
    // count) takes over: run 2's preflight sees est ≈ 10k tokens on a 3k-byte
    // history and compacts, where the heuristic's ~2250 would have skipped.
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.max_context_tokens = 20_000; // trip at 85% = 17_000
    cfg.limits.keep_recent_messages = 0;
    let cjk = "汉".repeat(1_000); // 3000 bytes → CJK-aware heuristic ≈ 2250
    let initial = vec![user_text(&cjk)];
    let (prov, state) = MockProvider::new(vec![
        mock::resp_text("r1", mock::mk_usage(10_000, 5)), // run 1: the provider measured the prompt at 10k tokens
        mock::resp_text("SUMMARY", None), // run 2 preflight compacts on the anchored estimate
        mock::resp_text("r2", None),      // run 2 main on the folded history
    ]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let mut events = Vec::new();
    agent.run(&mut |e| events.push(e)).unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compressed { .. })),
        "run 1: heuristic estimate (≈2250 + max_tokens) is under the cap — no compact"
    );
    // A follow-up turn (new run — the anchor must survive run boundaries,
    // TUI follow-ups being exactly where it pays).
    let _ = agent.push_user(vec![Block::Text("more".into())]);
    let mut ev2 = Vec::new();
    let o = agent.run(&mut |e| ev2.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(state.calls(), 3);
    assert!(
        ev2.iter().any(|e| matches!(
            e,
            AgentEvent::Compressed {
                trigger: CompressTrigger::Preflight,
                dropped: 2,
                kept: 1,
                ..
            }
        )),
        "run 2: anchored estimate (10k measured + delta) trips the compaction"
    );
}

#[test]
fn compact_between_runs_folds_reports_and_does_not_bill_run_totals() {
    // The host-facing idle entry: fold on demand (no threshold applies),
    // report dropped/kept, fire Compressed{trigger: Manual}, and hand the
    // summary's usage to the CALLER — self.usage resets at each run entry,
    // so folding it there would silently vanish (and it must not leak into
    // the next RunOutcome).
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.keep_recent_messages = 2;
    let initial = vec![
        user_text("task"),
        Message::assistant(vec![Block::Text("a1".into())]),
        user_text("u1"),
        Message::assistant(vec![Block::Text("a2".into())]),
        user_text("u2"),
    ];
    let (prov, state) = MockProvider::new(vec![mock::resp_text("SUMMARY", mock::mk_usage(3, 5))]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let mut events = Vec::new();
    let outcome = agent.compact(&mut |e| events.push(e)).unwrap();
    let o = outcome.expect("5-message history has a safe cut");
    assert_eq!(o.dropped, 2);
    assert_eq!(o.kept, 3);
    assert_eq!(
        o.usage,
        mock::mk_usage(3, 5),
        "summary billing returned to host"
    );
    assert_eq!(agent.messages.len(), 3);
    assert_eq!(state.calls(), 1);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Compressed {
            dropped: 2,
            kept: 3,
            trigger: CompressTrigger::Manual,
            ..
        }
    )));
    assert_eq!(
        agent.usage(),
        Usage::default(),
        "idle compact cost must NOT fold into run totals (they reset per run)"
    );
}

#[test]
fn compact_with_focus_hint_folds_like_plain_compact() {
    // The host's focus hint (Claude Code's `/compact focus on X`) threads
    // through to the summary prompt without changing the fold mechanics:
    // same dropped/kept, same manual-trigger event, same host-owned billing
    // — and the hint text verbatim rides the summarizer's user prompt
    // (Captured.user_texts records what the summary call was fed).
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.keep_recent_messages = 2;
    let initial = vec![
        user_text("task"),
        Message::assistant(vec![Block::Text("a1".into())]),
        user_text("u1"),
        Message::assistant(vec![Block::Text("a2".into())]),
        user_text("u2"),
    ];
    let (prov, state) = MockProvider::new(vec![mock::resp_text("SUMMARY", mock::mk_usage(3, 5))]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let mut events = Vec::new();
    let o = agent
        .compact_with(
            &mut |e| events.push(e),
            Some("focus on the API changes; keep every endpoint name"),
        )
        .unwrap()
        .expect("5-message history has a safe cut");
    assert_eq!(o.dropped, 2);
    assert_eq!(o.kept, 3);
    assert_eq!(state.calls(), 1);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Compressed {
            trigger: CompressTrigger::Manual,
            ..
        }
    )));
    let prompt = state.captured()[0].user_texts.concat();
    assert!(
        prompt.contains("## Additional instructions from the host"),
        "the hint section header rides the prompt: {prompt:?}"
    );
    assert!(
        prompt.contains("focus on the API changes"),
        "the hint text rides the prompt verbatim: {prompt:?}"
    );
    assert!(
        prompt.contains("Summarize this conversation transcript"),
        "the base summary prompt is still intact ahead of the hint"
    );
}

#[test]
fn with_focus_instructions_appends_hint_only_when_nonempty() {
    let base = summary_prompt("transcript", None);
    assert_eq!(with_focus_instructions(&base, None), base);
    assert_eq!(with_focus_instructions(&base, Some("   ")), base);
    let with = with_focus_instructions(&base, Some("keep API details"));
    assert!(with.starts_with(&base), "the hint appends, never rewrites");
    assert!(with.contains("## Additional instructions from the host"));
    assert!(with.contains("keep API details"));
}

#[test]
fn compact_archives_dropped_middle_to_session() {
    // Non-destructive compaction: the folded middle's ORIGINALS persist in
    // the session file's archive section (stable ids along), so a host can
    // audit or undo; the live history is the folded 3.
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.keep_recent_messages = 2;
    // Distinct temp-dir prefix from session.rs's archive roundtrip test
    // (`ta-arch-<pid>`): both end in remove_dir_all, and two same-binary tests
    // sharing one dir race each other's cleanup mid-write (a pre-existing
    // flake this rename removes from this side; session.rs is not ours to
    // touch).
    let dir = std::env::temp_dir().join(format!("ta-archmid-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.json");
    let initial = vec![
        user_text("task"),
        Message::assistant(vec![Block::Text("a1".into())]),
        user_text("u1"),
        Message::assistant(vec![Block::Text("a2".into())]),
        user_text("u2"),
    ];
    let (prov, _state) = MockProvider::new(vec![mock::resp_text("SUMMARY", None)]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    agent.enable_session(path.clone());
    let o = agent.compact(&mut |_| {}).unwrap().expect("folds");
    assert_eq!(o.dropped, 2);
    let (model, live, archive) = crate::session::load_with_archive(&path)
        .unwrap()
        .expect("session written");
    assert_eq!(model, "m");
    assert_eq!(live.len(), 3, "folded history");
    assert_eq!(archive.len(), 2, "the dropped middle archived");
    let archived_text: String = archive
        .iter()
        .flat_map(|m| {
            m.content.iter().filter_map(|b| match b {
                Block::Text(t) => Some(t.as_str()),
                _ => None,
            })
        })
        .collect();
    assert!(archived_text.contains("a1") && archived_text.contains("u1"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn prune_archives_originals_to_session() {
    // The prune rewrites tool results to placeholders in the LIVE history;
    // the original oversized content lands in the archive verbatim.
    let mut cfg = cfg_with(5, 0, 200);
    cfg.limits.keep_recent_messages = 2;
    let dir = std::env::temp_dir().join(format!("ta-prarch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.json");
    let mut initial = vec![user_text("task")];
    initial.extend(big_tool_turn("t1", "shell", 30_000));
    initial.push(user_text("u1"));
    initial.push(Message::assistant(vec![Block::Text("a2".into())]));
    initial.push(user_text("u2"));
    let (prov, _state) = MockProvider::new(vec![mock::resp_text("final answer", None)]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    agent.enable_session(path.clone());
    agent.run(&mut |_| {}).unwrap();
    let (_, live, archive) = crate::session::load_with_archive(&path)
        .unwrap()
        .expect("session written");
    // Live: placeholder; archive: the 30k original.
    let Block::ToolResult { content, .. } = &live[2].content[0] else {
        panic!("live tool result");
    };
    assert!(content.starts_with("[pruned tool output"));
    assert_eq!(archive.len(), 1);
    let Block::ToolResult { content: orig, .. } = &archive[0].content[0] else {
        panic!("archived tool result");
    };
    assert_eq!(
        orig.len(),
        30_000,
        "the original payload survives in the archive"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn archive_dedups_placeholder_when_a_later_fold_drops_it() {
    // Prune archives the ORIGINAL of a message whose placeholder stays live;
    // when a later fold drops that same message, the placeholder version
    // must NOT be filed next to the original — one id, one archived copy,
    // and it's the richer original.
    let mut cfg = cfg_with(5, 0, 30); // threshold so prune alone isn't enough
    cfg.limits.keep_recent_messages = 2;
    let dir = std::env::temp_dir().join(format!("ta-archdedup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.json");
    let mut initial = vec![user_text("task")];
    initial.extend(big_tool_turn("t1", "shell", 30_000));
    initial.push(user_text("u1"));
    initial.push(Message::assistant(vec![Block::Text("a2".into())]));
    initial.push(user_text("u2"));
    let (prov, _state) = MockProvider::new(vec![
        mock::resp_text("SUMMARY", None), // fold runs on top of the prune
        mock::resp_text("final answer", None), // main on folded history
    ]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    agent.enable_session(path.clone());
    let mut events = Vec::new();
    agent.run(&mut |e| events.push(e)).unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Pruned { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Compressed { .. })));
    let (_, _, archive) = crate::session::load_with_archive(&path)
        .unwrap()
        .expect("session written");
    let ids: Vec<&str> = archive.iter().filter_map(|m| m.id.as_deref()).collect();
    assert_eq!(
        ids.len(),
        ids.iter().collect::<std::collections::HashSet<_>>().len(),
        "no id filed twice: {ids:?}"
    );
    let trs: Vec<&String> = archive
        .iter()
        .flat_map(|m| {
            m.content.iter().filter_map(|b| match b {
                Block::ToolResult { content, .. } => Some(content),
                _ => None,
            })
        })
        .collect();
    assert_eq!(trs.len(), 1, "the message archived exactly once");
    assert_eq!(
        trs[0].len(),
        30_000,
        "and it's the original, not the placeholder"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn archive_byte_cap_evicts_the_oldest_until_the_total_fits() {
    // The count cap alone let a screenshot-dense session grow the archive —
    // and `save_session`'s per-turn full rewrite of it — without bound: three
    // archived ~30 MiB originals sit FAR under ARCHIVE_MAX_MESSAGES yet
    // together blow past the 64 MiB byte cap. The oldest must go first (same
    // FIFO as the count cap) until the ledger fits. Same-module access to the
    // private archive path: no provider script can carry 90 MiB of payload
    // through a full run cheaply, and the cap is exactly this function's
    // contract.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![], test_ctx());
    let payload = |tag: &str| {
        Message::user(vec![Block::ToolResult {
            tool_use_id: "t1".into(),
            content: format!("{tag}{}", "x".repeat(30 * 1024 * 1024)),
            image: None,
            is_error: false,
        }])
    };
    agent.archive_messages([payload("oldest:"), payload("middle:"), payload("newest:")]);
    // 3 × 30 MiB ≈ 94 MB > 64 MiB with only 3 entries — eviction here is
    // purely byte-driven, proving the byte cap (not the count cap) bound it.
    assert_eq!(agent.session_archive.len(), 2, "the oldest dropped");
    assert!(
        agent.session_archive_bytes <= ARCHIVE_MAX_BYTES,
        "ledger {} must fit the byte cap",
        agent.session_archive_bytes
    );
    // FIFO order preserved among the survivors: middle, then newest.
    let bodies: Vec<&str> = agent
        .session_archive
        .iter()
        .filter_map(|m| match &m.content[0] {
            Block::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[0].starts_with("middle:"), "oldest evicted first");
    assert!(bodies[1].starts_with("newest:"));
    // The push-time ledger is exactly the survivors' serialized sum — no
    // drift between what was accounted at archive time and what the eviction
    // walk subtracted.
    assert_eq!(
        agent.session_archive_bytes,
        agent
            .session_archive
            .iter()
            .map(archive_message_bytes)
            .sum::<usize>()
    );
}

#[test]
fn resume_mint_never_reuses_archived_ids() {
    // Fresh mints fill gaps by checking the LIVE history only; after a fold,
    // those gaps are exactly the archived messages' ids. Restoring the
    // archive must push the mint counter past it, so a post-resume
    // push_user can't issue an id already owned by an archived original.
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.keep_recent_messages = 2;
    let initial = vec![
        user_text("task"),
        Message::assistant(vec![Block::Text("a1".into())]),
        user_text("u1"),
        Message::assistant(vec![Block::Text("a2".into())]),
        user_text("u2"),
    ];
    let (prov, _state) = MockProvider::new(vec![mock::resp_text("SUMMARY", None)]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let outcome = agent.compact(&mut |_| {}).unwrap().expect("folds");
    assert_eq!(outcome.dropped, 2);
    let live: Vec<Message> = agent.messages().to_vec();
    // Simulate the resume path: new agent over the folded history, archive
    // restored (as main.rs does via load_with_archive + set_session_archive).
    let archive: Vec<Message> = agent.session_archive.clone();
    assert_eq!(archive.len(), 2);
    let (prov2, _s2) = MockProvider::new(vec![]);
    let mut resumed = Agent::new(&cfg, Box::new(prov2), vec![], live, test_ctx());
    resumed.set_session_archive(archive);
    let _ = resumed.push_user(vec![Block::Text("next task".into())]);
    let archived_ids: std::collections::HashSet<String> = resumed
        .session_archive
        .iter()
        .filter_map(|m| m.id.clone())
        .collect();
    let new_id = resumed.messages().last().unwrap().id.clone().unwrap();
    assert!(
        !archived_ids.contains(&new_id),
        "minted id {new_id} collides with an archived original ({archived_ids:?})"
    );
}

#[test]
fn compact_retries_when_reasoning_burns_the_whole_budget() {
    // TinyWorker acceptance #7: hybrid-thinking gateways (deepseek / glm)
    // bill reasoning_content against max_tokens; a long UPDATE-branch trace
    // can consume the ENTIRE budget → finish=length (mapped "max_tokens")
    // with ZERO body characters. That shape must retry ONCE with a doubled
    // budget instead of degrading to dropped=0 — and the burned attempt
    // still bills (usage merges across attempts).
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.keep_recent_messages = 2;
    let initial = vec![
        user_text("task"),
        Message::assistant(vec![Block::Text("a1".into())]),
        user_text("u1"),
        Message::assistant(vec![Block::Text("a2".into())]),
        user_text("u2"),
    ];
    let (prov, state) = MockProvider::new(vec![
        // Attempt 1: length-capped, zero body — the failure shape.
        MockTurn::Ok(ChatResponse {
            blocks: vec![],
            stop_reason: "max_tokens".into(),
            usage: mock::mk_usage(30, 1024),
        }),
        // Attempt 2 (doubled budget): trace + body fit, normal summary.
        mock::resp_text("SUMMARY", mock::mk_usage(20, 200)),
    ]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let mut events = Vec::new();
    let o = agent
        .compact(&mut |e| events.push(e))
        .unwrap()
        .expect("recovers");
    assert_eq!(state.calls(), 2, "exactly one retry");
    assert_eq!(o.dropped, 2, "compression happened — no degradation");
    assert_eq!(
        o.usage,
        mock::mk_usage(50, 1224),
        "both attempts bill honestly (30+20 in / 1024+200 out)"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Compressed {
            dropped: 2,
            trigger: CompressTrigger::Manual,
            ..
        }
    )));
    // The retry ran with the doubled budget.
    let caps = state.captured();
    assert_eq!(caps[0].max_tokens, caps[1].max_tokens / 2);
}

#[test]
fn compact_degrades_without_retry_on_empty_end_turn_summary() {
    // The retry targets ONLY the length-capped zero-body shape: a genuinely
    // empty reply (end_turn, no text) is the model's actual answer and still
    // takes the degradation branch on attempt 1 — no second request.
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.keep_recent_messages = 2;
    let initial = vec![
        user_text("task"),
        Message::assistant(vec![Block::Text("a1".into())]),
        user_text("u1"),
        Message::assistant(vec![Block::Text("a2".into())]),
        user_text("u2"),
    ];
    let (prov, state) = MockProvider::new(vec![MockTurn::Ok(ChatResponse {
        blocks: vec![],
        stop_reason: "end_turn".into(),
        usage: mock::mk_usage(10, 5),
    })]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let mut events = Vec::new();
    let o = agent
        .compact(&mut |e| events.push(e))
        .unwrap()
        .expect("degraded outcome");
    assert_eq!(state.calls(), 1, "no retry for end_turn empties");
    assert_eq!(o.dropped, 0, "degradation branch");
    assert_eq!(o.usage, mock::mk_usage(10, 5));
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Warning(w) if w.contains("empty summary"))));
}

#[test]
fn compact_accepts_a_truncated_but_nonempty_summary_first_try() {
    // A length-capped reply WITH body text is "truncated but coherent" — the
    // original acceptance still holds: no retry, fold the partial summary.
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.keep_recent_messages = 2;
    let initial = vec![
        user_text("task"),
        Message::assistant(vec![Block::Text("a1".into())]),
        user_text("u1"),
        Message::assistant(vec![Block::Text("a2".into())]),
        user_text("u2"),
    ];
    let (prov, state) = MockProvider::new(vec![MockTurn::Ok(ChatResponse {
        blocks: vec![Block::Text("partial summary…".into())],
        stop_reason: "max_tokens".into(),
        usage: mock::mk_usage(10, 1024),
    })]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let o = agent.compact(&mut |_| {}).unwrap().expect("folds");
    assert_eq!(state.calls(), 1, "non-empty body accepted on attempt 1");
    assert_eq!(o.dropped, 2);
}

#[test]
fn compact_returns_none_when_nothing_compressible() {
    // Short history → no safe cut → Ok(None), and no summary request fired.
    let cfg = cfg_with(5, 0, 0);
    let (prov, state) = MockProvider::new(vec![]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
    let outcome = agent.compact(&mut |_| {}).unwrap();
    assert!(outcome.is_none());
    assert_eq!(state.calls(), 0);
    assert_eq!(agent.messages.len(), 1);
}

#[test]
fn compact_failure_errors_and_a_retry_attempt_is_fresh() {
    // Err = the summary request itself failed. The stale compress_failed it
    // sets must NOT swallow the NEXT manual attempt — the run loop's
    // "disable for this run" semantics would otherwise turn the host's retry
    // button into a silent no-op.
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.keep_recent_messages = 2;
    let initial = vec![
        user_text("task"),
        Message::assistant(vec![Block::Text("a1".into())]),
        user_text("u1"),
        Message::assistant(vec![Block::Text("a2".into())]),
        user_text("u2"),
    ];
    let (prov, state) = MockProvider::new(vec![
        MockTurn::Err("mock: summary endpoint down".into()),
        MockTurn::Err("mock: summary endpoint down".into()), // transient retry fails too
        mock::resp_text("SUMMARY", None),
    ]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let err = agent.compact(&mut |_| {}).unwrap_err();
    assert!(err.to_string().contains("summary endpoint down"), "{err:#}");
    assert_eq!(agent.messages.len(), 5, "failed pass folds nothing");
    assert_eq!(state.calls(), 2, "blip + failed retry");
    // Second manual attempt is fresh (stale flag cleared at entry).
    let outcome = agent.compact(&mut |_| {}).unwrap();
    assert!(outcome.is_some(), "retry must not be swallowed");
    assert_eq!(agent.messages.len(), 3);
}

#[test]
fn compact_clears_a_stale_cancel_flag() {
    // A cancel left set by a previously-interrupted run would abort the
    // summary request before it starts (the streaming layer consults the
    // flag live). compact clears it at entry — the acceptance-point pattern
    // (run() never clears; whoever accepts new work does — here the host
    // explicitly starting a manual compact is accepting that work).
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.keep_recent_messages = 2;
    let initial = vec![
        user_text("task"),
        Message::assistant(vec![Block::Text("a1".into())]),
        user_text("u1"),
        Message::assistant(vec![Block::Text("a2".into())]),
        user_text("u2"),
    ];
    let (prov, _) = MockProvider::new(vec![mock::resp_text("SUMMARY", None)]);
    let flag = Arc::new(AtomicBool::new(false));
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    agent.set_cancel(flag.clone());
    flag.store(true, Ordering::Relaxed); // stale interrupt from an earlier run
    let outcome = agent.compact(&mut |_| {}).unwrap();
    assert!(
        outcome.is_some(),
        "stale cancel must not kill the summary call"
    );
    assert!(!flag.load(Ordering::Relaxed), "entry reset cleared it");
}

#[test]
fn tool_input_deltas_forward_live_and_stay_out_of_history() {
    // Streaming argument fragments surface as AgentEvent::ToolInputDelta
    // BEFORE the assembled ToolCall event, tagged with the call's id+name —
    // and are display feed only: the history carries the assembled ToolUse
    // input, never the fragment stream.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![
        MockTurn::OkToolStream {
            id: "c1".into(),
            name: "shell".into(),
            frags: vec!["{\"comma".into(), "nd\":\"ls\"}".into()],
        },
        mock::resp_text("done", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    let deltas: Vec<(String, String, String)> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolInputDelta { id, name, delta } => {
                Some((id.clone(), name.clone(), delta.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        deltas,
        vec![
            ("c1".into(), "shell".into(), "{\"comma".into()),
            ("c1".into(), "shell".into(), "nd\":\"ls\"}".into()),
        ]
    );
    // Fragment events precede the assembled ToolCall…
    let first_delta = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolInputDelta { .. }))
        .unwrap();
    let tool_call = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolCall { name, .. } if name == "shell"))
        .unwrap();
    assert!(
        first_delta < tool_call,
        "deltas stream before the call completes"
    );
    // …whose input is the joined, parsed object.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolCall { input, .. } if input["command"] == "ls"
    )));
    // History purity: the assistant turn holds the assembled ToolUse; no
    // fragment text appears as a standalone block anywhere.
    let assistant = &agent.messages[1];
    assert!(matches!(
        assistant.content.as_slice(),
        [Block::ToolUse { id, name, input }]
            if *id == "c1" && *name == "shell" && input["command"] == "ls"
    ));
}

#[test]
fn tool_result_turn_gets_an_id() {
    // The tool-result user turn is a NEW message entering history — it must
    // carry an anchor id like every other entry. Found by e2e (session file
    // had a None id at the tool_result turn); pins the fix.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![
        mock::resp_tool("c1", "echo", json!({}), None),
        mock::resp_text("done", None),
    ]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
    let _ = agent.run(&mut |_| {}).unwrap();
    assert_eq!(agent.messages.len(), 4);
    assert!(
        agent.messages.iter().all(|m| m.id.is_some()),
        "every history entry is anchored: {:?}",
        agent
            .messages
            .iter()
            .map(|m| m.id.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn message_ids_minted_on_ingest_and_unique() {
    // Ingest-point assignment: construction/ingest (try_new), the run loop's
    // assistant turn, and push_user each mint a unique id. A follow-up that
    // MERGES into a trailing user turn keeps that turn's id (no new message
    // entered the history).
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![mock::resp_text("ok", None)]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
    assert_eq!(agent.messages[0].id.as_deref(), Some("m1"));
    let _ = agent.run(&mut |_| {}).unwrap();
    let assistant_id = agent.messages[1].id.clone();
    assert!(assistant_id.is_some(), "assistant turn minted an id");
    assert_ne!(assistant_id.as_deref(), Some("m1"));
    // Follow-up after an assistant turn → a new user message, new id.
    let _ = agent.push_user(vec![Block::Text("more".into())]);
    let followup_id = agent.messages[2].id.clone();
    assert!(followup_id.is_some());
    assert_ne!(followup_id, assistant_id);
    // All ids pairwise distinct.
    let ids: Vec<&str> = agent
        .messages
        .iter()
        .map(|m| m.id.as_deref().unwrap())
        .collect();
    let mut uniq = ids.clone();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(ids.len(), uniq.len(), "no id repeats");
    // Merge case: pushing while the history ends in user keeps the id.
    let before = agent.messages.last().unwrap().id.clone();
    let _ = agent.push_user(vec![Block::Text("again".into())]);
    assert_eq!(agent.messages.last().unwrap().id, before);
    assert_eq!(agent.messages.len(), 3, "merged, not appended");
}

#[test]
fn resumed_message_ids_preserved_and_not_reissued() {
    // A history carrying ids (resumed save / host-supplied) keeps them
    // verbatim, and fresh mints skip past them — a process restart must
    // never re-issue an id the saved lineage already uses.
    let cfg = cfg_with(5, 0, 0);
    let (prov, _) = MockProvider::new(vec![]);
    let mut resumed = Message::assistant(vec![Block::Text("a1".into())]);
    resumed.id = Some("m9".into());
    let mut opener = user_text("task");
    opener.id = Some("m5".into());
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![opener, resumed],
        test_ctx(),
    );
    assert_eq!(agent.messages[0].id.as_deref(), Some("m5"));
    assert_eq!(agent.messages[1].id.as_deref(), Some("m9"));
    let _ = agent.push_user(vec![Block::Text("follow-up".into())]);
    let minted = agent.messages[2].id.clone().expect("new message minted");
    assert_ne!(minted.as_str(), "m5");
    assert_ne!(minted.as_str(), "m9");
}

#[test]
fn fresh_message_id_terminates_when_counter_saturated_and_max_id_taken() {
    // Termination pin for the mint loop's degenerate corner (external
    // re-review P2): counter at u64::MAX AND "m{MAX}" already occupied by a
    // (necessarily resumed — no real history mints 2^64 ids) lineage. The
    // old loop regenerated the same colliding candidate forever; the fix
    // breaks out by returning that candidate. This test FAILS AS A HANG on
    // the old code, which is exactly what it pins: the call must RETURN.
    let mut max_id_holder = Message::assistant(vec![Block::Text("stale".into())]);
    max_id_holder.id = Some(format!("m{}", u64::MAX));
    let mut next = u64::MAX;
    let id = fresh_message_id(&[max_id_holder], &mut next);
    assert_eq!(id, format!("m{}", u64::MAX));
    // The non-degenerate neighbor still behaves: counter at MAX-1 with the
    // MAX id taken mints m{MAX} normally (collision-free single advance).
    let mut next = u64::MAX - 1;
    let id = fresh_message_id(&[], &mut next);
    assert_eq!(id, format!("m{}", u64::MAX));
    assert_eq!(next, u64::MAX);
}

#[test]
fn compression_keeps_tail_ids_and_mints_new_entries() {
    // Folding the middle never renumbers the survivors: the kept head keeps
    // its id, the tail messages keep theirs verbatim (anchors don't drift),
    // and the fold's fresh entries (summary carrier / fresh head) get mints
    // so the post-compress history is fully anchored.
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.keep_recent_messages = 2;
    let initial = vec![
        user_text("task"),
        Message::assistant(vec![Block::Text("a1".into())]),
        user_text("u1"),
        Message::assistant(vec![Block::Text("a2".into())]),
        user_text("u2"),
    ];
    let (prov, _) = MockProvider::new(vec![mock::resp_text("SUMMARY", None)]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let ids_before: Vec<Option<String>> = agent.messages.iter().map(|m| m.id.clone()).collect();
    let outcome = agent.compact(&mut |_| {}).unwrap().expect("folds");
    assert_eq!(outcome.kept, 3);
    assert_eq!(agent.messages.len(), 3);
    // Head survives with its id; the tail (messages[3..] → folded [1..])
    // keeps ids verbatim; everything is anchored.
    assert_eq!(agent.messages[0].id, ids_before[0]);
    assert_eq!(agent.messages[1].id, ids_before[3]);
    assert_eq!(agent.messages[2].id, ids_before[4]);
    assert!(
        agent.messages.iter().all(|m| m.id.is_some()),
        "post-compress history fully anchored"
    );
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
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Compressed {
                dropped: 4,
                kept: 3,
                ..
            }
        )),
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
        mock::resp_text("SUMMARY", None),      // summarize_prefix call
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
        mock::resp_text("SUMMARY", None), // summary ran, no usage reported
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
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Compressed {
            dropped: 2,
            kept: 3,
            ..
        }
    )));
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
        AgentEvent::Usage {
            input_tokens: 100,
            output_tokens: 10,
            ..
        }
    ));
}

#[test]
fn compress_failure_degrades_gracefully() {
    // summarize_prefix errors → compress_failed set, NO Compressed event,
    // the run continues uncompressed and still completes. TWO consecutive
    // errors: the summary call retries one transient blip with backoff
    // (see summary_call_retries_one_transient_failure), so the disable path
    // needs the retry to fail too.
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
        MockTurn::Err("summary blew up again".into()), // retry fails too
        mock::resp_text("answer anyway", None),  // main call proceeds uncompressed
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
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Compressed {
            dropped: 2,
            kept: 3,
            ..
        }
    )));
    let cap = state.captured();
    assert_eq!(cap[1].message_count, 3, "main call saw the folded history");
}

// ── PDF document blocks: page-estimate heuristic & preflight ─────────────

/// base64 of `bytes` (test helper for building Document blocks).
fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A minimal fake PDF: a page-tree node + `pages` plain page objects.
fn fake_pdf(pages: usize, with_markers: bool, tree_count: Option<usize>) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n".to_vec();
    let kids = (0..pages)
        .map(|i| format!("{} 0 R", i + 2))
        .collect::<Vec<_>>()
        .join(" ");
    match tree_count {
        Some(n) => out.extend_from_slice(
            format!("1 0 obj << /Type /Pages /Kids [{kids}] /Count {n} >> endobj\n").as_bytes(),
        ),
        None => out.extend_from_slice(
            format!("1 0 obj << /Type /Pages /Kids [{kids}] >> endobj\n").as_bytes(),
        ),
    }
    if with_markers {
        for i in 0..pages {
            out.extend_from_slice(format!("{} 0 obj << /Type /Page >> endobj\n", i + 2).as_bytes());
        }
    }
    out
}

#[test]
fn pdf_page_estimate_counts_plain_page_objects() {
    // 3 uncompressed page objects (the common LaTeX/browser/Word shape) → 3.
    assert_eq!(pdf_page_estimate(&b64(&fake_pdf(3, true, None))), 3);
}

#[test]
fn pdf_page_estimate_excludes_pages_tree_nodes() {
    // The `/Type /Pages` PARENT node must not count as a page: tree + 2 page
    // objects (no /Count to cross-check) → 2, not 3. Also covers the no-space
    // `/Type/Page` spelling.
    let raw = fake_pdf(2, true, None);
    assert_eq!(pdf_page_estimate(&b64(&raw)), 2);
    let nospace = b"%PDF-1.4\n1 0 obj << /Type/Page >> endobj\n2 0 obj << /Type/Page >> endobj\n";
    assert_eq!(pdf_page_estimate(&b64(nospace)), 2);
}

#[test]
fn pdf_page_estimate_recovers_count_when_markers_are_hidden() {
    // ObjStm-compressed PDFs (1.5+) can hide page objects from a byte scan;
    // the page-tree /Count is the recovery path. Marker count (0) < /Count 5.
    assert_eq!(pdf_page_estimate(&b64(&fake_pdf(5, false, Some(5)))), 5);
    // Mixed: markers say 2 but the tree says 5 → trust the larger.
    assert_eq!(pdf_page_estimate(&b64(&fake_pdf(5, true, Some(5)))), 5);
    // A large /Kids array (~7 bytes/page) pushes /Count past a small forward
    // window — 600 page refs ≈ 4 KiB of Kids before the count shows up. The
    // window must still find it (large documents are where the count matters).
    let kids = (2..602)
        .map(|i| format!("{i} 0 R"))
        .collect::<Vec<_>>()
        .join(" ");
    let big = format!("%PDF-1.5\n1 0 obj << /Type /Pages /Kids [{kids}] /Count 600 >> endobj\n");
    assert_eq!(pdf_page_estimate(&b64(big.as_bytes())), 600);
}

#[test]
fn pdf_page_estimate_falls_back_to_size_for_opaque_bytes() {
    // No markers, no tree → ~100 KiB/page fallback, floored at 1.
    let raw = vec![b'x'; 250 * 1024]; // → 2 pages
    assert_eq!(pdf_page_estimate(&b64(&raw)), 2);
    let small = b"%PDF-1.4 opaque stream bytes";
    assert_eq!(pdf_page_estimate(&b64(small)), 1);
    // Undecodable base64 → treated as 1 page (an estimate, never an error).
    assert_eq!(pdf_page_estimate("!!not-base64!!"), 1);
}

#[test]
fn estimate_tokens_counts_pdf_by_pages_not_bytes() {
    // A 3-page PDF estimates at 3 × PDF_TOKENS_PER_PAGE regardless of payload
    // size — a bytes-per-token heuristic would under-count a dense text PDF
    // by orders of magnitude.
    let doc = Block::Document {
        media_type: "application/pdf".into(),
        data: b64(&fake_pdf(3, true, None)),
    };
    let msgs = vec![Message::user(vec![doc, Block::Text("summarize".into())])];
    let est = estimate_tokens(&msgs, None, 0);
    // Expected from the same inputs total_chars uses: the doc's char floor
    // (base64 len / 100) + the text, over CHARS_PER_TOKEN, plus pages ×
    // PDF_TOKENS_PER_PAGE.
    let doc_floor = match &msgs[0].content[0] {
        Block::Document { data, .. } => data.len() / 100,
        _ => unreachable!(),
    };
    let expected = (doc_floor + "summarize".len()) / CHARS_PER_TOKEN + 3 * PDF_TOKENS_PER_PAGE;
    assert_eq!(est, expected);
    // Same payload with NO page structure visible still counts (fallback ≥1).
    let opaque = Block::Document {
        media_type: "application/pdf".into(),
        data: b64(b"%PDF-1.4 tiny"),
    };
    let msgs = vec![Message::user(vec![opaque])];
    assert!(estimate_tokens(&msgs, None, 0) >= PDF_TOKENS_PER_PAGE);
}

#[test]
fn pdf_page_estimate_matches_a_real_producer_pdf() {
    // Guard against the heuristic drifting away from REAL PDF structure: the
    // fixture is genuine `cupsfilter` output (CUPS/macOS text pipeline, 3
    // pages — verified against PDFKit's pageCount when committed), not the
    // synthetic `fake_pdf` used elsewhere. Synthetic fixtures pin the
    // heuristic's LOGIC (marker exclusion, /Count recovery, fallbacks); this
    // one pins that real producers' byte layouts still land in the scanned
    // structures. (Measured when introduced: exact match on CUPS 3p/46p and
    // Quartz image-PDF 1p; ObjStm-hidden page objects — Word/Acrobat output —
    // remain covered only synthetically, no such producer ran offline.)
    use base64::Engine;
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/cups_3p.pdf");
    let raw = std::fs::read(path).expect("committed fixture cups_3p.pdf");
    let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
    assert_eq!(pdf_page_estimate(&b64), 3);
}

#[test]
fn pdf_estimate_drives_preflight_compression() {
    // A document block's page estimate must trip the same 85%-of-window
    // preflight gate a long text does: 3 pages × 3000 tokens > 85% of a
    // 10k window → compress BEFORE the main request.
    let mut cfg = cfg_with(5, 0, 0); // char threshold off
    cfg.limits.max_context_tokens = 10_000;
    cfg.limits.keep_recent_messages = 2;
    cfg.max_tokens = 100;
    let initial = vec![
        Message::user(vec![
            Block::Document {
                media_type: "application/pdf".into(),
                data: b64(&fake_pdf(3, true, None)),
            },
            Block::Text("the task".into()),
        ]),
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
        .any(|e| matches!(e, AgentEvent::Compressed { .. })));
    // The summarizer transcript shows the document as a marker line, and the
    // main call saw the folded (3-message) history.
    let cap = state.captured();
    assert!(
        cap[0].user_texts.iter().any(|t| t.contains("<document>")),
        "summary transcript marks the doc"
    );
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
        mock::resp_text("SUMMARY", None), // force-compress summary call
        mock::resp_text("recovered answer", None), // retried main call
    ]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(o.final_text, "recovered answer");
    // overflow Err → forced summary → retried main. Exactly 3 calls.
    assert_eq!(state.calls(), 3);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Compressed {
                dropped: 2,
                kept: 3,
                trigger: CompressTrigger::Overflow,
                ..
            }
        )),
        "forced compression after the overflow error must be tagged overflow"
    );
    let cap = state.captured();
    assert_eq!(
        cap[0].message_count, 5,
        "first (overflowing) call saw full history"
    );
    assert_eq!(
        cap[2].message_count, 3,
        "retried call saw the folded history"
    );
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
    let (prov, state) = MockProvider::new(vec![MockTurn::Err(
        "OpenAI-compatible API error 429: rate limit exceeded".into(),
    )]);
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
    let (prov, state) = MockProvider::new(vec![MockTurn::Err(
        "Anthropic API error 400: prompt is too long".into(),
    )]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let err = match agent.run(&mut |_| {}) {
        Err(e) => e,
        Ok(_) => panic!("overflow with no compressible head must propagate"),
    };
    assert!(format!("{err:#}").contains("prompt is too long"));
    assert_eq!(
        state.calls(),
        1,
        "no summary call when nothing is compressible"
    );
}

#[test]
fn is_context_overflow_distinguishes_input_from_output_token_limit() {
    // Unambiguous context-overflow phrases match (compressing may help).
    assert!(is_context_overflow(&anyhow::anyhow!(
        "context_length_exceeded"
    )));
    assert!(is_context_overflow(&anyhow::anyhow!("prompt is too long")));
    // A bare "token limit exceeded" is some gateways' INPUT-overflow wording
    // → matches (compressing may help).
    assert!(is_context_overflow(&anyhow::anyhow!(
        "token limit exceeded"
    )));
    // …but the same phrase in an OUTPUT / completion context must NOT match —
    // compressing the input won't fix a max-output-tokens error, and treating
    // it as overflow wastes one force-compress + retry that fails identically.
    assert!(!is_context_overflow(&anyhow::anyhow!(
        "max output token limit exceeded"
    )));
    assert!(!is_context_overflow(&anyhow::anyhow!(
        "completion token limit exceeded"
    )));
    // Control: unrelated errors never match.
    assert!(!is_context_overflow(&anyhow::anyhow!(
        "rate limit exceeded"
    )));
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
    head.content.push(Block::Text(format!(
        "{SUMMARY_MARKER}\nold running summary body"
    )));
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
    assert!(
        !transcript.contains(SUMMARY_MARKER),
        "marker block stripped"
    );
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
    assert!(
        !cap[0].cache && !cap[1].cache && !cap[2].cache,
        "summaries cache:false"
    );
    assert!(cap[3].cache, "main call uses prompt caching");
    // The main call saw the folded (short) history.
    assert_eq!(cap[3].message_count, 3);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Compressed {
            dropped: 2,
            kept: 3,
            ..
        }
    )));
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
        MockTurn::Err("chunk2 summary blew up".into()), // transient retry fails too
        mock::resp_text("answer anyway", None),         // main proceeds uncompressed
    ]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(o.final_text, "answer anyway");
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, AgentEvent::Compressed { .. })),
        "no fold on chunk failure"
    );
    let cap = state.captured();
    assert_eq!(
        cap[3].message_count, 5,
        "no fold → main sees the full history"
    );
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
    let (prov, _) = MockProvider::new(vec![MockTurn::Err("503 service unavailable".into())]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
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
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
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
fn stream_error_past_deadline_ends_run_as_timeout() {
    // External re-review P2: the Ok-shaped stream failure (stop_reason
    // "stream_error") that only materializes once the run's wall clock is
    // gone must take the SAME deadline vocabulary as the Err arm
    // (request_error_past_deadline_ends_run_as_timeout): clean "timeout"
    // stop_reason (exit 124 downstream), partial usage billed, and — the
    // point of the fix — NOT a retryable Err that would replay the whole
    // task on a fresh budget (measured 2 requests / ~2.8s with one retry
    // before the fix).
    let cfg = cfg_with(5, 1, 0);
    let (prov, _) = MockProvider::new(vec![MockTurn::OkAfter {
        delay: std::time::Duration::from_millis(1200),
        resp: ChatResponse {
            blocks: vec![Block::Text("[stream error] operation timed out".into())],
            stop_reason: "stream_error".into(),
            usage: mock::mk_usage(1500, 7),
        },
    }]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "timeout");
    assert_eq!(o.turns, 1);
    // The partial usage survived the vocabulary switch: billed in the
    // outcome, not dropped by the timeout conversion.
    assert_eq!(
        (o.input_tokens, o.output_tokens),
        (1500, 7),
        "stream_error-at-deadline partial usage must stay billed"
    );
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
    let mut msgs = vec![
        user_text("a"),
        Message::assistant(vec![Block::Text("x".into())]),
    ];
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
        duration_ms: None,
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
            duration_ms: None,
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
        duration_ms: None,
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
    assert_eq!(
        updates,
        vec![1],
        "exactly one TodoUpdate with the 1-item plan"
    );
}

#[test]
fn system_with_env_prepends_block_and_keeps_base() {
    // base present → env block first, blank-line separator, then the caller
    // prompt in full.
    let s = system_with_env(Some("You are a coder."), "macos", "sh -c", "/tmp/proj").expect("Some");
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

#[test]
fn normalize_tool_use_ids_mints_missing_and_renumbers_duplicates() {
    // Pure-function form of the repair: empty ids (gateways that omit the
    // field) get a minted one, duplicates get renumbered, and unique
    // non-empty ids survive untouched.
    let mut blocks = vec![
        Block::Text("preamble".into()),
        Block::ToolUse {
            id: "".into(),
            name: "shell".into(),
            input: json!({}),
        },
        Block::ToolUse {
            id: "dup".into(),
            name: "shell".into(),
            input: json!({}),
        },
        Block::ToolUse {
            id: "dup".into(),
            name: "shell".into(),
            input: json!({}),
        },
        Block::ToolUse {
            id: "real-1".into(),
            name: "shell".into(),
            input: json!({}),
        },
    ];
    normalize_tool_use_ids(&mut blocks, &[]);
    let ids: Vec<&str> = blocks
        .iter()
        .filter_map(|b| match b {
            Block::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 4);
    assert_eq!(ids[3], "real-1", "unique non-empty id is kept as-is");
    assert!(
        ids.iter().all(|i| !i.is_empty()),
        "no empty id survives: {ids:?}"
    );
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 4, "all ids distinct after repair: {ids:?}");
}

#[test]
fn normalize_tool_use_ids_seeds_seen_from_history() {
    // Cross-turn collision (pure form): history already carries
    // `toolu_repair_0_0` from a previous repair; a new response with a broken
    // id at the SAME block index must not re-mint that id — the deterministic
    // `{i}_{n}` naming made `seen`-only-scans-current-response collide on the
    // second broken turn, and the duplicate then rode every later request.
    let history = vec![Message::assistant(vec![Block::ToolUse {
        id: "toolu_repair_0_0".into(),
        name: "shell".into(),
        input: json!({}),
    }])];
    let mut blocks = vec![Block::ToolUse {
        id: "".into(),
        name: "shell".into(),
        input: json!({}),
    }];
    normalize_tool_use_ids(&mut blocks, &history);
    let Block::ToolUse { id, .. } = &blocks[0] else {
        panic!("tool use block");
    };
    assert_eq!(id, "toolu_repair_0_1", "mint must dodge the historical id");
    // A provider id repeated ACROSS turns is also a duplicate now: the copy
    // gets renumbered against the history instead of sailing through.
    let mut again = vec![Block::ToolUse {
        id: "provider-1".into(),
        name: "shell".into(),
        input: json!({}),
    }];
    let history2 = vec![Message::assistant(vec![Block::ToolUse {
        id: "provider-1".into(),
        name: "shell".into(),
        input: json!({}),
    }])];
    normalize_tool_use_ids(&mut again, &history2);
    let Block::ToolUse { id, .. } = &again[0] else {
        panic!("tool use block");
    };
    assert_eq!(id, "toolu_repair_0_0", "cross-turn duplicate is renumbered");
}

#[test]
fn loop_two_broken_turns_mint_distinct_repair_ids() {
    // Loop form of the same regression (#2, 2026-09 review): a gateway that
    // omits the id two turns in a row used to mint the SAME
    // `toolu_repair_0_0` in both turns — the history then carried a duplicate
    // no later repair could fix (every later request 400s on Anthropic).
    let cfg = cfg_with(5, 0, 0);
    let echo = EchoTool::new("shell");
    let (prov, state) = MockProvider::new(vec![
        MockTurn::Ok(ChatResponse {
            blocks: vec![Block::ToolUse {
                id: "".into(),
                name: "shell".into(),
                input: json!({"command": "ls"}),
            }],
            stop_reason: "tool_use".into(),
            usage: None,
        }),
        MockTurn::Ok(ChatResponse {
            blocks: vec![Block::ToolUse {
                id: "".into(),
                name: "shell".into(),
                input: json!({"command": "pwd"}),
            }],
            stop_reason: "tool_use".into(),
            usage: None,
        }),
        mock::resp_text("done", None),
    ]);
    let initial = vec![user_text("run these")];
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(echo)],
        initial,
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).expect("run completes");
    assert_eq!(o.stop_reason, "end_turn");
    // The THIRD request carries both minted ids (turn 1's in history, turn
    // 2's fresh) — they must be distinct and 1:1 paired with results.
    let cap = state.captured();
    let third = &cap[2];
    let uses = third.tool_use_ids.clone();
    assert_eq!(
        uses.len(),
        2,
        "both turns' tool_use ids on request 3: {uses:?}"
    );
    assert!(
        uses.iter().all(|i| i.starts_with("toolu_repair_")),
        "both broken ids were minted: {uses:?}"
    );
    assert_ne!(uses[0], uses[1], "cross-turn mint collision: {uses:?}");
    let mut results = third.tool_result_ids.clone();
    let mut sorted = uses.clone();
    sorted.sort_unstable();
    results.sort_unstable();
    assert_eq!(sorted, results, "N:N bijection across the two minted ids");
}

#[test]
fn loop_pairs_results_after_id_repair() {
    // End-to-end: a gateway response with one empty id and two colliding
    // ids used to produce ambiguous tool_use ↔ tool_result pairing (all
    // three results "matched" the wrong use; Anthropic 400s). The loop
    // repairs ids BEFORE the assistant message is persisted, so the next
    // request must show a clean N:N bijection with no empty ids.
    let cfg = cfg_with(5, 0, 0);
    let echo = EchoTool::new("shell");
    let (prov, state) = MockProvider::new(vec![
        MockTurn::Ok(ChatResponse {
            blocks: vec![
                Block::ToolUse {
                    id: "".into(),
                    name: "shell".into(),
                    input: json!({"command": "ls"}),
                },
                Block::ToolUse {
                    id: "dup".into(),
                    name: "shell".into(),
                    input: json!({"command": "pwd"}),
                },
                Block::ToolUse {
                    id: "dup".into(),
                    name: "shell".into(),
                    input: json!({"command": "true"}),
                },
            ],
            stop_reason: "tool_use".into(),
            usage: mock::mk_usage(10, 5),
        }),
        mock::resp_text("done", mock::mk_usage(3, 3)),
    ]);
    let initial = vec![user_text("run these")];
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![Box::new(echo)],
        initial,
        test_ctx(),
    );
    let o = agent.run(&mut |_| {}).expect("run completes");
    assert_eq!(o.stop_reason, "end_turn");
    assert_eq!(o.tool_calls.len(), 3, "all three repaired calls executed");
    // Second request: history must carry 3 distinct non-empty tool_use ids
    // with exactly matching tool_results.
    let cap = state.captured();
    let second = &cap[1];
    let mut uses = second.tool_use_ids.clone();
    let mut results = second.tool_result_ids.clone();
    assert!(uses.iter().all(|i| !i.is_empty()), "ids: {uses:?}");
    uses.sort_unstable();
    results.sort_unstable();
    assert_eq!(uses, results, "N:N bijection after repair");
    uses.dedup();
    assert_eq!(uses.len(), 3, "all three ids distinct: {uses:?}");
}

#[test]
fn run_tool_truncates_error_text_to_output_cap() {
    // A tool Err becomes a model-visible ToolResult, so an arbitrarily long
    // error string (e.g. from an MCP server) must be capped at
    // max_output_chars like any other tool output — it used to bypass the
    // cap entirely, letting a hostile/verbose server bloat every later
    // request until the context overflowed.
    struct ExplodeTool;
    impl Tool for ExplodeTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: "explode".into(),
                description: "always errors, verbosely".into(),
                schema: json!({"type": "object", "properties": {}}),
            }
        }
        fn run(&self, _input: &Value, _ctx: &ToolCtx) -> anyhow::Result<ToolOutput> {
            anyhow::bail!("{}", "E".repeat(5000));
        }
    }
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(ExplodeTool)];
    let defs: Vec<ToolDef> = tools.iter().map(|t| t.def()).collect();
    let ctx = test_ctx(); // max_output_chars = 1000
    let out = run_tool(&tools, &defs, "explode", &json!({}), &ctx, &mut |_| {});
    assert!(out.is_error);
    let n = out.content.chars().count();
    assert!(
        n <= 1000 + 40, // cap + the "[truncated…]" marker
        "error text must be capped (got {n} chars)"
    );
    assert!(out.content.contains("[truncated"), "{}", out.content);
}

#[test]
fn huge_timeout_secs_does_not_panic_on_clock_overflow() {
    // Instant + Duration panics on platforms whose clock it overflows
    // (macOS: u64 nanoseconds ≈ 584 years; u64::MAX seconds blows past it).
    // A timeout that large is "no deadline" for practical purposes — the
    // run must complete normally instead of panicking at run entry.
    let cfg = cfg_with(3, u64::MAX, 0);
    let (prov, state) = MockProvider::new(vec![mock::resp_text("ok", mock::mk_usage(2, 2))]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], vec![], test_ctx());
    let o = agent.run(&mut |_| {}).expect("run completes");
    assert_eq!(o.stop_reason, "end_turn");
    // The per-request streaming backstop stays armed even with no run
    // deadline — the reader-thread leak guard must never go away.
    assert!(
        state.captured()[0].per_request_timeout.is_some(),
        "STREAM_BACKSTOP must still cap the request"
    );
}

// ── run_code e2e (requires the `code` feature; design §13 A-1) ─────────────
//
// The loop-level contract: a model-direct `run_code` call drives the wasm
// guest, whose own `tools.call` produces INNER ToolCall/ToolResult events in
// the parent stream while the outer call returns the guest's JSON result as
// its tool output. Everything runs offline (MockProvider + real wasm).

#[cfg(feature = "code")]
#[test]
fn run_code_a1_loop_round_trip() {
    use crate::tools::run_code::RunCodeTool;
    use std::sync::RwLock;

    let dir = std::env::temp_dir().join(format!("ta-run-code-a1-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note.txt"), "hello-a1\n").unwrap();

    let cfg = cfg_with(5, 0, 0);
    let run_code = Box::new(RunCodeTool::new(
        &cfg,
        Arc::new(RwLock::new(cfg.code.clone())),
        &reqwest::blocking::Client::new(),
        "k",
        vec![],
        vec![],
    ));
    let (prov, state) = MockProvider::new(vec![
        mock::resp_tool(
            "t1",
            "run_code",
            json!({"code": "const r = readAll(\"note.txt\");\n\
                          const list = tools.list();\n\
                          return {head: r.slice(0, 30), knows: list.some(t => t.name === \"read_file\")};"}),
            None,
        ),
        mock::resp_text("done", None),
    ]);
    let ctx = ToolCtx {
        tool_absent_notes: Vec::new(),
        workdir: dir.clone(),
        max_output_chars: 2000,
        restrict_to_workdir: false,
        cancel: None,
        shell_timeout_secs: 5,
        hooks: None,
        subagent: None,
        budget: None,
        deadline: None,
    };
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![run_code],
        vec![user_text("go")],
        ctx,
    );
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");

    // Outer result = the guest's JSON return value (A-1's core assertion:
    // the sandbox round-trips data through a real tool call).
    let outer = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                name,
                is_error: false,
                content,
            } if name == "run_code" => Some(content.clone()),
            _ => None,
        })
        .expect("outer run_code ToolResult event");
    assert!(outer.contains("hello-a1"), "{outer}");
    assert!(outer.contains("\"knows\":true"), "{outer}");

    // Inner events reached the parent stream, paired and ordered:
    // ToolCall(read_file) < ToolResult(read_file) < ToolResult(run_code).
    let pos = |needle: &str, is_result: bool| {
        events.iter().position(|e| match e {
            AgentEvent::ToolCall { name, .. } => !is_result && name == needle,
            AgentEvent::ToolResult { name, .. } => is_result && name == needle,
            _ => false,
        })
    };
    let inner_call = pos("read_file", false).expect("inner ToolCall event");
    let inner_result = pos("read_file", true).expect("inner ToolResult event");
    let outer_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolResult { name, .. } if name == "run_code"))
        .unwrap();
    assert!(inner_call < inner_result);
    assert!(
        inner_result < outer_pos,
        "inner result precedes the outer one"
    );

    // The second request pairs the OUTER run_code tool_use with exactly one
    // tool_result (inner calls live inside run_code's result, not as blocks).
    let cap = &state.captured()[1];
    assert_eq!(cap.tool_use_ids, cap.tool_result_ids);
    assert_eq!(cap.trailing_tool_results, 1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A-2 (US-1, the CSV story): the guest loops over paged reads, aggregates
/// in-memory, writes the CSV via write_file, and returns a compact summary —
/// one tool call doing what used to cost a turn per page.
#[cfg(feature = "code")]
#[test]
fn run_code_a2_csv_pipeline() {
    use crate::tools::run_code::RunCodeTool;
    use std::sync::RwLock;

    // Hermetic half of US-1: the read→aggregate→write pipeline over paged
    // FILES (read_file paging stands in for the mock DB tool — no hermetic
    // McpTool exists). The MCP leg the real scenario uses is pinned by
    // run_code::tests::guest_reaches_mcp_tools_live (#[ignore]).
    let dir = std::env::temp_dir().join(format!("ta-run-code-a2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // Five "pages" of `name,amount` rows (the read_file line-number prefix is
    // stripped in-guest by splitting on the first tab — the F-2 contract).
    let mut expected = std::collections::BTreeMap::new();
    for p in 1..=5 {
        let mut page = String::new();
        for r in 0..4 {
            let name = format!("cust-{}", p * 10 + r);
            let amount = p * 100 + r;
            page.push_str(&format!("{name},{amount}\n"));
            *expected.entry(name).or_insert(0u64) += amount;
        }
        std::fs::write(dir.join(format!("page{p}.txt")), page).unwrap();
    }

    let cfg = cfg_with(5, 0, 0);
    let run_code = Box::new(RunCodeTool::new(
        &cfg,
        Arc::new(RwLock::new(cfg.code.clone())),
        &reqwest::blocking::Client::new(),
        "k",
        vec![],
        vec![],
    ));
    let (prov, _) = MockProvider::new(vec![
        mock::resp_tool(
            "t1",
            "run_code",
            json!({"code": r#"
                let totals = {};
                for (let p = 1; p <= 5; p++) {
                    const t = tools.call("read_file", {path: "page" + p + ".txt"});
                    for (const line of t.split("\n")) {
                        const tab = line.indexOf("\t");
                        if (tab < 0) continue;
                        const [name, amount] = line.slice(tab + 1).split(",");
                        totals[name] = (totals[name] || 0) + Number(amount);
                    }
                }
                const lines = ["name,total",
                    ...Object.entries(totals).map(([n, v]) => n + "," + v)];
                tools.call("write_file", {path: "totals.csv", content: lines.join("\n")});
                return {file: "totals.csv", customers: Object.keys(totals).length};"#}),
            None,
        ),
        mock::resp_text("done", None),
    ]);
    let ctx = ToolCtx {
        tool_absent_notes: Vec::new(),
        workdir: dir.clone(),
        max_output_chars: 4000,
        restrict_to_workdir: false,
        cancel: None,
        shell_timeout_secs: 5,
        hooks: None,
        subagent: None,
        budget: None,
        deadline: None,
    };
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![run_code],
        vec![user_text("aggregate")],
        ctx,
    );
    let mut events = Vec::new();
    let o = agent.run(&mut |e| events.push(e)).unwrap();
    assert_eq!(o.stop_reason, "end_turn");

    // The CSV landed with correct sums…
    let csv = std::fs::read_to_string(dir.join("totals.csv")).unwrap();
    assert!(csv.starts_with("name,total"), "{csv}");
    for (name, total) in &expected {
        assert!(csv.contains(&format!("{name},{total}")), "{csv}");
    }
    // …the outer tool result is the guest's compact summary…
    let outer = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                name,
                is_error: false,
                content,
            } if name == "run_code" => Some(content.clone()),
            _ => None,
        })
        .expect("outer run_code result");
    assert!(outer.contains("\"customers\":20"), "{outer}");
    // …and the audit trace counted all 6 inner calls (5 reads + 1 write).
    let rec = o.tool_calls.iter().find(|c| c.name == "run_code").unwrap();
    let meta = rec.meta.as_ref().expect("run_code audit meta");
    assert_eq!(meta["call_count"], 6, "{meta}");
    let inner_ok = meta["calls"].as_array().unwrap();
    assert!(inner_ok.iter().all(|c| c["ok"] == true), "{meta}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A-3 / test 14 (authorization equivalence): under the SAME hooks layer, a
/// model-direct read_file and a guest-internal read_file produce equivalent
/// gate decisions and after-notify sequences — same tool, same input, same
/// executed outcome; the ONLY difference is the ctx's sub-agent marker
/// (`-` at top level vs `__code__` from the sandbox).
#[cfg(feature = "code")]
#[test]
fn run_code_a3_gate_sequence_equivalence() {
    use crate::tools::run_code::RunCodeTool;
    use std::sync::RwLock;

    let dir = std::env::temp_dir().join(format!("ta-run-code-a3-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note.txt"), "shared-note\n").unwrap();

    let cfg = cfg_with(6, 0, 0);
    let run_code = Box::new(RunCodeTool::new(
        &cfg,
        Arc::new(RwLock::new(cfg.code.clone())),
        &reqwest::blocking::Client::new(),
        "k",
        vec![],
        vec![],
    ));
    let (prov, _) = MockProvider::new(vec![
        // Turn 1: the MODEL calls read_file directly.
        mock::resp_tool("t1", "read_file", json!({"path": "note.txt"}), None),
        // Turn 2: the MODEL calls run_code, whose guest calls the same tool.
        mock::resp_tool(
            "t2",
            "run_code",
            json!({"code": r#"return tools.call("read_file", {path: "note.txt"}).trim();"#}),
            None,
        ),
        mock::resp_text("done", None),
    ]);
    let ctx = ToolCtx {
        tool_absent_notes: Vec::new(),
        workdir: dir.clone(),
        max_output_chars: 2000,
        restrict_to_workdir: false,
        cancel: None,
        shell_timeout_secs: 5,
        hooks: None,
        subagent: None,
        budget: None,
        deadline: None,
    };
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![run_code],
        vec![user_text("go")],
        ctx,
    );
    let hooks = Arc::new(CaptureHooks::new());
    agent.set_hooks(hooks.clone());
    let o = agent.run(&mut |_| {}).unwrap();
    assert_eq!(o.stop_reason, "end_turn");

    // Gate order and inputs are IDENTICAL for both origins; only the marker
    // differs (the last field in each "before:tool:input:tag" entry).
    let befores: Vec<String> = hooks
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("before:read_file:"))
        .collect();
    assert_eq!(befores.len(), 2, "{befores:?}");
    let top = befores[0].clone();
    let inner = befores[1].clone();
    assert!(top.ends_with(":-"), "top-level marker: {top}");
    assert!(inner.ends_with(":__code__"), "guest marker: {inner}");
    let drop_last = |s: &str| match s.rfind(':') {
        Some(i) => s[..i].to_string(),
        None => s.to_string(),
    };
    assert_eq!(
        drop_last(&top),
        drop_last(&inner),
        "same tool + same input on both origins"
    );
    // Both actually executed (Allow) — after_tool with executed=true twice.
    let afters: Vec<String> = hooks
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("after:read_file:true:"))
        .collect();
    assert_eq!(afters.len(), 2, "{afters:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A-10a / test 16a (switch face): enabled=false keeps run_code out of the
/// model's view; a static config enable exposes it; the runtime setter
/// round-trips; an explicit whitelist mention without the enable errors with
/// the remedy.
#[cfg(feature = "code")]
#[test]
fn run_code_a10a_switch_face() {
    let base = r#"{"provider":"openai","api_key_env":"MOCK_API_KEY","model":"m",
        "inject_environment":false,"tools":["read_file"]}"#;

    // 1. Default (enabled=false): not in the tool list, model cannot see it.
    let cfg: Config = serde_json::from_str(base).unwrap();
    let agent = crate::build_agent_with_provider(
        &cfg,
        Box::new(MockProvider::new(vec![mock::resp_text("x", None)]).0),
        vec![],
        &reqwest::blocking::Client::new(),
        vec![],
        "k",
    )
    .unwrap();
    assert!(!agent.tool_names().contains(&"run_code".to_string()));

    // 2. Explicit whitelist mention without the enable → actionable error.
    let cfg: Config = serde_json::from_str(
        r#"{"provider":"openai","api_key_env":"MOCK_API_KEY","model":"m",
            "inject_environment":false,"tools":["read_file","run_code"]}"#,
    )
    .unwrap();
    let err = match crate::build_agent_with_provider(
        &cfg,
        Box::new(MockProvider::new(vec![mock::resp_text("x", None)]).0),
        vec![],
        &reqwest::blocking::Client::new(),
        vec![],
        "k",
    ) {
        Err(e) => e,
        Ok(_) => panic!("explicit run_code listing without code.enabled must be rejected"),
    };
    assert!(
        format!("{err:#}").contains("code.enabled"),
        "error must name the switch: {err:#}"
    );

    // 3. Static enable: visible …
    let mut cfg: Config = serde_json::from_str(base).unwrap();
    cfg.code.enabled = true;
    let mut agent = crate::build_agent_with_provider(
        &cfg,
        Box::new(MockProvider::new(vec![mock::resp_text("x", None)]).0),
        vec![],
        &reqwest::blocking::Client::new(),
        vec![],
        "k",
    )
    .unwrap();
    assert!(agent.tool_names().contains(&"run_code".to_string()));

    // 4. …and the runtime round-trip flips it (refresh covers the defs the
    // next request would serialize).
    agent.set_code_enabled(false).unwrap();
    assert!(!agent.tool_names().contains(&"run_code".to_string()));
    agent.set_code_enabled(true).unwrap();
    assert!(agent.tool_names().contains(&"run_code".to_string()));
    // set_code_config's enabled flip does the same table sync.
    let mut tuned = cfg.code.clone();
    tuned.enabled = false;
    agent.set_code_config(tuned).unwrap();
    assert!(!agent.tool_names().contains(&"run_code".to_string()));
    // 5. And the enabled instance is callable end-to-end (the A-1 loop
    // already covers the full run; here we just prove dispatch finds it).
    agent.set_code_enabled(true).unwrap();
    let names = agent.tool_names();
    assert!(names.contains(&"run_code".to_string()) && names.contains(&"read_file".to_string()));
}

/// Switch face on a hand-built agent (no `set_code_tool` install — the
/// feature-off / direct-`try_new` shape): every switch/config call errors
/// with the same actionable message (review: the None branch had no pin).
#[cfg(feature = "code")]
#[test]
fn run_code_switch_face_none_branch() {
    let cfg = cfg_with(3, 0, 0);
    let mut agent = Agent::new(
        &cfg,
        Box::new(MockProvider::new(vec![mock::resp_text("x", None)]).0),
        vec![],
        vec![user_text("hi")],
        test_ctx(),
    );
    for call in ["enable", "disable", "config"] {
        let r = match call {
            "enable" => agent.set_code_enabled(true),
            "disable" => agent.set_code_enabled(false),
            _ => agent.set_code_config(cfg.code.clone()),
        };
        let err = r.unwrap_err();
        assert!(
            format!("{err:#}").contains("no run_code tool installed"),
            "{call}: actionable error, got: {err:#}"
        );
    }
}

/// External audit P2-5: set_code_config is a trusted config entrance — it
/// runs the same llm tier invariants the load-time path enforces BEFORE
/// replacing (a default outside the allowlist used to install silently,
/// and the first omitted-model llm call dispatched to the unlisted model).
#[cfg(feature = "code")]
#[test]
fn set_code_config_rejects_invalid_llm_tier() {
    let cfg: Config = serde_json::from_str(
        r#"{"provider":"openai","api_key_env":"MOCK_API_KEY","model":"m",
            "inject_environment":false,"tools":["read_file"],"code":{"enabled":true}}"#,
    )
    .unwrap();
    let mut agent = crate::build_agent_with_provider(
        &cfg,
        Box::new(MockProvider::new(vec![mock::resp_text("x", None)]).0),
        vec![],
        &reqwest::blocking::Client::new(),
        vec![],
        "k",
    )
    .unwrap();
    assert!(agent.tool_names().contains(&"run_code".to_string()));

    // 1. default outside the allowlist → Err naming the invariant.
    let mut bad = cfg.code.clone();
    bad.llm.enabled = true;
    bad.llm.default_model = Some("outside".into());
    bad.llm.models = vec!["allowed".into()];
    let err = agent.set_code_config(bad).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("code.llm.default_model") && msg.contains("code.llm.models"),
        "actionable error naming the invariant: {msg}"
    );

    // 2. enabled with an empty allowlist → Err.
    let mut bad = cfg.code.clone();
    bad.llm.enabled = true;
    bad.llm.default_model = Some("m1".into());
    bad.llm.models = vec![];
    let err = agent.set_code_config(bad).unwrap_err();
    assert!(
        format!("{err:#}").contains("non-empty code.llm.models"),
        "{}",
        format!("{err:#}")
    );

    // 3. The failed writes left the installed config untouched — the tool
    // table still agrees with the original enabled state.
    assert!(agent.tool_names().contains(&"run_code".to_string()));

    // 4. A valid tier (with duplicate spellings) installs cleanly.
    let mut good = cfg.code.clone();
    good.llm.enabled = true;
    good.llm.default_model = Some("m1".into());
    good.llm.models = vec!["m1".into(), "m1".into(), "m2".into()];
    agent.set_code_config(good).unwrap();
    assert!(agent.tool_names().contains(&"run_code".to_string()));
}

/// LD-14 snapshot boundary, made audible: once the sandbox registry is
/// built, `set_code_config` can no longer change what guest `llm` calls
/// see — before this pin the update returned Ok and silently did nothing.
/// The warning fires only when the llm half actually differs, so ordinary
/// hot updates (limits, paths, enabled) stay quiet.
#[cfg(feature = "code")]
#[test]
fn set_code_config_warns_when_llm_was_pinned_by_the_registry() {
    use std::sync::Mutex;

    let cfg: Config = serde_json::from_str(
        r#"{"provider":"openai","api_key_env":"MOCK_API_KEY","model":"m",
            "inject_environment":false,"tools":["read_file"],"code":{"enabled":true}}"#,
    )
    .unwrap();
    let mut agent = crate::build_agent_with_provider(
        &cfg,
        Box::new(MockProvider::new(vec![mock::resp_text("x", None)]).0),
        vec![],
        &reqwest::blocking::Client::new(),
        vec![],
        "k",
    )
    .unwrap();
    // First execution boundary: the registry caches its tool table (without
    // the llm primitive, since the gate was off at build time).
    agent
        .code_tool
        .as_ref()
        .unwrap()
        .ensure_registry_for_test()
        .unwrap();

    let warnings = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&warnings);
    let _guard = crate::diag::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::diag::install_diag_sink(Box::new(move |level, msg| {
        if level == crate::diag::DiagLevel::Warn {
            sink.lock().unwrap().push(msg.to_string());
        }
    }));

    // A non-llm hot update: no freeze warning. (Filter by content: the sink
    // is process-global, so unrelated tests' skill/assembly warnings can land
    // here too.)
    let mut quiet = cfg.code.clone();
    quiet.max_tool_calls = quiet.max_tool_calls.saturating_add(1);
    agent.set_code_config(quiet).unwrap();
    let llm_warnings = |w: &[String]| w.iter().filter(|m| m.contains("code.llm")).count();
    let quiet_seen = warnings.lock().unwrap().clone();
    assert_eq!(
        llm_warnings(&quiet_seen),
        0,
        "limits-only update must not warn: {quiet_seen:?}"
    );

    // Enabling the guest llm primitive after the freeze: warn.
    let mut frozen = cfg.code.clone();
    frozen.llm.enabled = true;
    frozen.llm.default_model = Some("m1".into());
    frozen.llm.models = vec!["m1".into()];
    agent.set_code_config(frozen.clone()).unwrap();
    let frozen_seen = warnings.lock().unwrap().clone();
    assert_eq!(
        llm_warnings(&frozen_seen),
        1,
        "exactly one freeze warning, got {frozen_seen:?}"
    );
    let msg = frozen_seen.iter().find(|m| m.contains("code.llm")).unwrap();
    assert!(
        msg.contains("Rebuild the agent"),
        "warning must name the freeze and the remedy: {msg}"
    );

    // Re-applying the SAME config is not a change: no repeat warning.
    agent.set_code_config(frozen).unwrap();
    let repeat_seen = warnings.lock().unwrap().clone();
    assert_eq!(llm_warnings(&repeat_seen), 1, "{repeat_seen:?}");
    crate::diag::clear_diag_sink();
}

// ── context_breakdown: composition, calibration, attribution ─────────────
//
// The breakdown's own arithmetic (running-floor partition, per-bucket sums)
// is pinned in context_report.rs's unit tests; these cover the AGENT side —
// where the parts come from (fields, anchor, tool table) and the anchor's
// lifecycle around runs and folds.

#[test]
fn context_breakdown_uncalibrated_matches_estimate() {
    // Before any billed run there is no anchor: pure heuristic, parts sum
    // to EXACTLY estimate_tokens over the same (messages, system, tools).
    let cfg = cfg_with(5, 0, 0);
    let initial = vec![
        user_text(&"中文历史内容估算".repeat(30)),
        Message::assistant(vec![Block::Text("reply text".into())]),
    ];
    let agent = Agent::new(
        &cfg,
        Box::new(MockProvider::new(vec![]).0),
        vec![],
        initial,
        test_ctx(),
    );
    let bd = agent.context_breakdown();
    assert!(bd.calibration.is_none(), "no billed run yet");
    let expected = crate::compress::estimate_tokens(
        &agent.messages,
        agent.system.as_deref(),
        agent.tool_schema_chars,
    ) as u64;
    assert_eq!(bd.total_tokens, expected);
    assert_eq!(
        bd.system.tokens + bd.tools.tokens + bd.messages.tokens,
        expected
    );
    // The two text kinds carry the whole (text-only) history.
    assert_eq!(
        bd.messages.by_kind.user_text + bd.messages.by_kind.assistant_text,
        bd.messages.tokens
    );
    // cfg_with has inject_environment=false and no system prompt: the whole
    // system section is empty.
    assert_eq!(bd.system.tokens, 0);
}

#[test]
fn context_breakdown_calibrates_after_billed_run() {
    // CJK-heavy history: the CJK-aware heuristic is much closer than bytes/4,
    // but the provider's tokenizer still disagrees, so the anchor's measured/
    // estimated scale exceeds 1 and the breakdown's components scale past
    // their raw estimates. The anchor survives the run boundary (a follow-up
    // push_user keeps the prefix intact — that's the TUI's main use).
    let cfg = cfg_with(5, 0, 0);
    let cjk = "中文内容估算校准锚点".repeat(150); // 1500 chars → ~3375 raw est
    let (prov, state) = MockProvider::new(vec![mock::resp_text("done", mock::mk_usage(10_000, 5))]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text(&cjk)],
        test_ctx(),
    );
    agent.run(&mut |_| {}).unwrap();
    assert_eq!(state.calls(), 1);

    // The anchor's raw denominator is the PREFIX AS REQUESTED (the initial
    // user message), not the post-run history — recompute it that way.
    let anchor = agent.usage_anchor.as_ref().expect("anchor written");
    let raw_prefix = crate::compress::estimate_tokens(
        &agent.messages[..anchor.msg_count],
        agent.system.as_deref(),
        agent.tool_schema_chars,
    ) as u64;
    let bd = agent.context_breakdown();
    let cal = bd.calibration.expect("billed run leaves a live anchor");
    assert_eq!(cal.measured_prompt_tokens, 10_000);
    assert!(
        cal.scale > 1.0,
        "provider tokenizer disagrees with the CJK-aware heuristic, scale={}",
        cal.scale
    );
    assert_eq!(cal.scale, 10_000.0 / raw_prefix as f64);
    // The scaled total towers over the raw estimate (that's the point of
    // calibrating) and still splits across the three sections.
    assert!(
        bd.total_tokens > raw_prefix * 2,
        "scaled={}, raw={}",
        bd.total_tokens,
        raw_prefix
    );
    assert_eq!(cal.scale, anchor.scale);

    // Follow-up message: prefix unchanged → anchor (and calibration) stays.
    agent.push_user(vec![Block::Text("one more".into())]);
    assert!(agent.context_breakdown().calibration.is_some());
}

#[test]
fn zero_prompt_usage_writes_no_anchor() {
    // A usage object whose input axes are all zero is a gateway that did
    // NOT report prompt usage (seen in the wild: relays omitting
    // usage.prompt_tokens, Anthropic-protocol gateways sending
    // {input_tokens: 0}). Zero prompt tokens is impossible for a real
    // request; treating it as a measurement pinned the anchor scale to
    // 0.0 — a "calibrated" all-zero breakdown — and degraded
    // estimate_input_tokens to 0 + delta. Skip the anchor instead: the
    // pull stays pure-heuristic and the preflight estimate falls back to
    // the raw estimate. Output tokens still bill honestly.
    let cfg = cfg_with(5, 0, 0);
    let (prov, state) = MockProvider::new(vec![mock::resp_text("done", mock::mk_usage(0, 5))]);
    let mut agent = Agent::new(
        &cfg,
        Box::new(prov),
        vec![],
        vec![user_text("a real prompt with some content")],
        test_ctx(),
    );
    agent.run(&mut |_| {}).unwrap();
    assert_eq!(state.calls(), 1);

    assert!(
        agent.usage_anchor.is_none(),
        "zero prompt usage must not pin an anchor"
    );
    let bd = agent.context_breakdown();
    assert!(bd.calibration.is_none());
    assert!(
        bd.total_tokens > 0,
        "heuristic estimate, not the scaled-zero lie"
    );
    assert_eq!(
        bd.system.tokens + bd.tools.tokens + bd.messages.tokens,
        bd.total_tokens
    );
    // Billing is untouched by the anchor decision.
    assert_eq!(agent.usage().output_tokens, 5);
}

#[test]
fn calibration_voids_on_fold() {
    // A manual compact rewrites the history without a main-chat re-anchor:
    // the anchor's prefix no longer matches and the breakdown falls back to
    // the pure heuristic (calibration None), still summing exactly.
    let mut cfg = cfg_with(5, 0, 0);
    cfg.limits.keep_recent_messages = 2;
    let initial = vec![
        user_text("task one with enough body to summarize meaningfully"),
        Message::assistant(vec![Block::Text("did one".into())]),
        user_text("task two with enough body to summarize meaningfully"),
        Message::assistant(vec![Block::Text("did two".into())]),
        user_text("task three with enough body to summarize meaningfully"),
        Message::assistant(vec![Block::Text("did three".into())]),
        user_text("latest question"),
    ];
    let (prov, state) = MockProvider::new(vec![
        mock::resp_text("done", mock::mk_usage(500, 1)),
        mock::resp_text("SUMMARY", mock::mk_usage(300, 1)),
    ]);
    let mut agent = Agent::new(&cfg, Box::new(prov), vec![], initial, test_ctx());
    agent.run(&mut |_| {}).unwrap();
    assert!(agent.context_breakdown().calibration.is_some());

    let out = agent
        .compact(&mut |_| {})
        .unwrap()
        .expect("history compressible");
    assert!(out.dropped > 0);
    assert_eq!(state.calls(), 2);
    let bd = agent.context_breakdown();
    assert!(bd.calibration.is_none(), "fold rewrote the prefix");
    let expected = crate::compress::estimate_tokens(
        &agent.messages,
        agent.system.as_deref(),
        agent.tool_schema_chars,
    ) as u64;
    assert_eq!(bd.total_tokens, expected);
}

#[test]
fn tool_origin_buckets_and_hot_remove() {
    // The four origin buckets each hold their tools; an MCP hot-remove
    // rebuilds the table, empties the mcp bucket, and (tool_schema_chars
    // changed) voids any anchor — here just the raw re-sum.
    let cfg = cfg_with(5, 0, 0);
    let echo = crate::tools::CallbackTool::new(
        ToolDef {
            name: "echo".into(),
            description: "test callback".into(),
            schema: serde_json::json!({"type": "object"}),
        },
        |_, _| Ok(ToolOutput::text("ok")),
    );
    let (index, _) = crate::skills::scan(&[]);
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(echo) as Box<dyn Tool>,
        Box::new(FakeOriginMcpTool),
        Box::new(crate::tools::ReadSkill::new(index.clone())),
        Box::new(crate::tools::ListSkill::new(index)),
    ];
    let mut agent = Agent::new(
        &cfg,
        Box::new(MockProvider::new(vec![]).0),
        tools,
        vec![],
        test_ctx(),
    );
    let names = |bd: &crate::context_report::ContextBreakdown, b: &str| -> Vec<String> {
        let cat = match b {
            "builtin" => &bd.tools.builtin,
            "mcp" => &bd.tools.mcp,
            "skill" => &bd.tools.skill,
            _ => &bd.tools.custom,
        };
        cat.tools.iter().map(|t| t.name.clone()).collect()
    };
    let bd = agent.context_breakdown();
    assert_eq!(names(&bd, "custom"), ["echo"]);
    assert_eq!(names(&bd, "mcp"), ["fake_mcp"]);
    assert_eq!(names(&bd, "skill"), ["read_skill", "list_skill"]);
    assert!(names(&bd, "builtin").is_empty());

    agent.remove_mcp_server("fake_server").unwrap();
    let bd = agent.context_breakdown();
    assert!(
        bd.tools.mcp.tools.is_empty(),
        "hot remove emptied the bucket"
    );
    assert_eq!(names(&bd, "skill"), ["read_skill", "list_skill"]);
    // The remaining buckets still sum against the live tool table.
    assert_eq!(
        bd.tools.tokens,
        bd.tools.builtin.tokens
            + bd.tools.mcp.tokens
            + bd.tools.skill.tokens
            + bd.tools.custom.tokens
    );
}

/// A minimal tool that claims the MCP origin + server the hot-remove path
/// routes by (capability-marker style, like the real McpTool).
#[derive(Default)]
struct FakeOriginMcpTool;

impl Tool for FakeOriginMcpTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "fake_mcp".into(),
            description: "fake".into(),
            schema: serde_json::json!({"type": "object"}),
        }
    }
    fn run(&self, _input: &Value, _ctx: &ToolCtx) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::text("ok"))
    }
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Mcp
    }
    fn mcp_server(&self) -> Option<&str> {
        Some("fake_server")
    }
}

#[test]
fn env_attribution_splits_env_from_base() {
    // inject_environment=true: the env header is its own segment, the user's
    // prompt is the base, and no intro exists on direct construction
    // (skill_intro == 0 — the default attribution is correct without the
    // assembly-time setter).
    let cfg = serde_json::from_str::<Config>(
        r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":true,"system_prompt":"You are a careful test agent."}"#,
    )
    .unwrap();
    let agent = Agent::new(
        &cfg,
        Box::new(MockProvider::new(vec![]).0),
        vec![],
        vec![],
        test_ctx(),
    );
    let bd = agent.context_breakdown();
    assert!(bd.system.env > 0, "env header attributed");
    assert!(bd.system.base > 0, "user prompt attributed");
    assert_eq!(bd.system.skill_intro, 0, "direct try_new has no intro");
    assert_eq!(
        bd.system.env + bd.system.base + bd.system.skill_intro,
        bd.system.tokens
    );

    // inject_environment=false: everything after (nothing) — env == 0 and
    // the base carries the whole prompt.
    let cfg = serde_json::from_str::<Config>(
        r#"{"provider":"openai","api_key_env":"K","model":"m","inject_environment":false,"system_prompt":"You are a careful test agent."}"#,
    )
    .unwrap();
    let agent = Agent::new(
        &cfg,
        Box::new(MockProvider::new(vec![]).0),
        vec![],
        vec![],
        test_ctx(),
    );
    let bd = agent.context_breakdown();
    assert_eq!(bd.system.env, 0);
    assert_eq!(bd.system.skill_intro, 0);
    assert_eq!(bd.system.base, bd.system.tokens);
    assert!(bd.system.tokens > 0);
}

/// The NDJSON vocabulary is a public contract (`--json` pipelines and both
/// bindings key on the `type` tag), and the harness side cannot enforce it:
/// it sees `AgentEvent` as `#[non_exhaustive]` and must keep a `_` arm. The
/// kernel-side `ndjson_type()` is exhaustive, so a NEW variant won't compile
/// until it is named there; this test pins the names themselves (a rename is
/// just as breaking as a missing one) and covers every variant so a future
/// variant that IS named but never exercised here gets caught by the count.
#[test]
fn every_agent_event_has_a_stable_ndjson_type_tag() {
    let cases: Vec<(AgentEvent, &str)> = vec![
        (AgentEvent::TurnStart(1), "turn"),
        (AgentEvent::TextDelta(String::new()), "text"),
        (AgentEvent::ThinkingDelta(String::new()), "thinking"),
        (AgentEvent::CodeLog(String::new()), "code_log"),
        (
            AgentEvent::ToolInputDelta {
                id: String::new(),
                name: String::new(),
                delta: String::new(),
            },
            "tool_input",
        ),
        (
            AgentEvent::ToolCall {
                name: String::new(),
                input: Value::Null,
            },
            "tool_call",
        ),
        (
            AgentEvent::ToolResult {
                name: String::new(),
                is_error: false,
                content: String::new(),
            },
            "tool_result",
        ),
        (AgentEvent::TodoUpdate { todos: Vec::new() }, "todo"),
        (
            AgentEvent::Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
            "usage",
        ),
        (
            AgentEvent::Compressed {
                dropped: 0,
                kept: 0,
                trigger: CompressTrigger::Preflight,
                estimated_tokens_before: 0,
                estimated_tokens_after: 0,
                measured_tokens_before: None,
            },
            "compressed",
        ),
        (
            AgentEvent::Pruned {
                messages: 0,
                chars_freed: 0,
            },
            "pruned",
        ),
        (AgentEvent::MaxTokensSalvage, "salvage"),
        (AgentEvent::Warning(String::new()), "warning"),
        (
            AgentEvent::SubAgent {
                name: String::new(),
                event: Box::new(AgentEvent::Done {
                    stop_reason: String::new(),
                }),
            },
            "subagent",
        ),
        (
            AgentEvent::Done {
                stop_reason: String::new(),
            },
            "done",
        ),
    ];
    let mut seen = std::collections::BTreeSet::new();
    for (ev, want) in &cases {
        assert_eq!(ev.ndjson_type(), *want, "wrong NDJSON tag for {ev:?}");
        assert!(seen.insert(*want), "duplicate NDJSON tag {want}");
    }
    // Guards against a variant added to the enum but not to this table: the
    // table must name as many distinct tags as the enum has variants. (The
    // compile-time exhaustiveness lives in `ndjson_type` itself; this is the
    // test-side mirror that also catches a "named but never yielded" arm.)
    assert_eq!(seen.len(), 15, "update the table when AgentEvent grows");
}
