//! Context composition breakdown: how the NEXT request's context splits into
//! estimated tokens by source — system prompt (env header / user base / skill
//! intro), tool schemas (builtin / mcp / skill / custom), and message history
//! (by block kind). Built for hosts that visualize context usage (the TUI's
//! `/context` and binding-side equivalents of Claude Code's context report).
//!
//! Yardstick discipline: every number here is a HEURISTIC ESTIMATE (text
//! units/4 — ASCII 1 unit, CJK 9 units — image payloads len/100, PDF pages ×
//! 3000 — the same rules [`crate::compress::estimate_tokens`] uses), never a
//! provider measurement.
//! The billing authority is always `AgentEvent::Usage` / [`Agent::usage`]
//! (crate::agent::Agent::usage). That is why the breakdown is a pull-only
//! read query ([`crate::agent::Agent::context_breakdown`]) and NOT an event:
//! the Usage event is the measured register, this is the estimated register,
//! and the two deliberately never share a stream.
//!
//! Sum invariant: with `calibration == None`,
//! `system.tokens + tools.tokens + messages.tokens == total_tokens ==
//! compress::estimate_tokens(messages, system, tool_schema_chars)` — held
//! EXACTLY by allocating every chars-segment its running-floor share of ONE
//! division by [`CHARS_PER_TOKEN`] over the total (each segment gets
//! `floor(acc_after/4) - floor(acc_before/4)`). Dividing each segment
//! independently would drift the sum by a token here and there; the invariant
//! is load-bearing for tests and for hosts that reconcile the breakdown
//! against the preflight estimate.
//!
//! Calibration ([`Calibration`]) multiplies every component by one uniform
//! `scale` — the anchored request's measured/estimated ratio. That is itself
//! a heuristic (it assumes the estimate error — CJK undercount, JSON
//! overhead — is proportionally similar across components), and after
//! scaling the components may differ from their totals by rounding.

use crate::compress::{
    block_chars, block_pdf_tokens, char_units, estimate_tokens, image_payload_len, text_units,
    CHARS_PER_TOKEN,
};
use crate::provider::{Block, Message, Role, ToolDef};
use crate::tools::{Tool, ToolOrigin};
use serde::Serialize;

/// The estimated token composition of the next request's context. Every
/// number is an estimate (see the module docs); `calibration` says which
/// register the numbers are in.
#[derive(Debug, Clone, Serialize)]
pub struct ContextBreakdown {
    /// System prompt split into its three sources.
    pub system: SystemBreakdown,
    /// Tool schema cost split into four origin buckets, per tool.
    pub tools: ToolsBreakdown,
    /// Message history aggregated by block kind.
    pub messages: MessagesBreakdown,
    /// Everything summed: `system.tokens + tools.tokens + messages.tokens`
    /// (exactly, when uncalibrated; up to rounding when scaled).
    pub total_tokens: u64,
    /// `None` = pure heuristic (no valid measured anchor yet); `Some` = the
    /// components were uniformly scaled by the anchor's measured/estimated
    /// ratio.
    pub calibration: Option<Calibration>,
}

/// The system prompt's three sources. Separator bytes (the `"\n\n"` after
/// the env header, the marker before the skill intro) belong to the
/// ADJACENT segment so that `env + base + skill_intro == tokens` exactly.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SystemBreakdown {
    /// The `# Environment` header (OS / Shell / cwd) `Agent::try_new` prepends
    /// when `inject_environment` is on. `0` when off.
    pub env: u64,
    /// The config's own system prompt (its length BEFORE assembly appended
    /// the skill intro).
    pub base: u64,
    /// The level-1 skill intro `assemble` appends when skills are installed.
    /// Non-zero only on the `build_agent*` path; agents built by direct
    /// `Agent::try_new` (including `task` sub-agents, which don't inherit
    /// skills) have no intro.
    pub skill_intro: u64,
    /// The three parts summed.
    pub tokens: u64,
}

/// One origin bucket of tool-schema cost.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ToolCategory {
    pub tokens: u64,
    /// Per-tool entries in tool-list order (the same order
    /// `Agent::tool_names` reports, not sorted).
    pub tools: Vec<ToolEntry>,
}

/// One tool's estimated schema cost: `(name + description + schema JSON)
/// chars / 4` — the same per-tool yardstick `Agent`'s `tool_schema_chars`
/// sums over.
#[derive(Debug, Clone, Serialize)]
pub struct ToolEntry {
    pub name: String,
    pub tokens: u64,
}

/// Tool schema cost by [`ToolOrigin`]. The per-tool chars here sum to the
/// same total `Agent::tool_schema_chars` caches.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ToolsBreakdown {
    pub builtin: ToolCategory,
    pub mcp: ToolCategory,
    pub skill: ToolCategory,
    pub custom: ToolCategory,
    /// The four buckets summed.
    pub tokens: u64,
}

/// Message history aggregated by block kind.
#[derive(Debug, Clone, Serialize)]
pub struct MessagesBreakdown {
    pub tokens: u64,
    pub by_kind: MessageKinds,
}

/// Message-history kinds. There is deliberately NO `thinking` bucket:
/// TinyAgent's thinking is a pure event stream (`ThinkingDelta`) that never
/// enters history (the `Block` enum has no variant for it), so there is
/// structurally nothing to count — a permanently-zero field would be a
/// lying API. If a `Block::Thinking` variant ever lands, the classifier's
/// exhaustive match fails at compile time and its author places it in
/// `other` first, promoting it to a named field when warranted.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MessageKinds {
    /// `Text` blocks inside user messages.
    pub user_text: u64,
    /// `Text` blocks inside assistant messages.
    pub assistant_text: u64,
    /// `ToolUse` argument JSON (except `read_skill` — see `skill_content`).
    pub tool_use: u64,
    /// `ToolResult` text content (except `read_skill` results). An attached
    /// image payload counts as `image`, not here.
    pub tool_result: u64,
    /// Top-level `Image` blocks + `ToolResult` image payloads (base64
    /// len/100).
    pub image: u64,
    /// PDF `Document` blocks: their chars/4 share PLUS pages × 3000 (the
    /// double bill is exactly what `estimate_tokens` does).
    pub document: u64,
    /// `read_skill`'s tool_use arguments and tool_result bodies — the
    /// skill bodies actually loaded into history (level 2/3 content),
    /// including the synthetic eager-injection pair assembly can prepend.
    /// Attributed so hosts can draw one "skills" slice covering intro +
    /// bodies. `list_skill` does NOT count (a small listing is ordinary
    /// tool traffic).
    pub skill_content: u64,
    /// Fallback for `Block` variants that don't map to a named bucket. The
    /// classifier matches exhaustively (in-crate `#[non_exhaustive]` doesn't
    /// apply), so a new variant fails that match at compile time and its
    /// author must place it — here until it earns a named bucket.
    pub other: u64,
}

/// Calibration info for a scaled breakdown. `scale` is the anchor request's
/// (provider-measured prompt tokens) ÷ (same-prefix `estimate_tokens`
/// estimate). When the anchor is still valid, [`Agent::context_breakdown`]
/// multiplies every component by it.
///
/// Uniform scaling is a HEURISTIC — it assumes the estimate error is
/// proportionally similar across components, which is usually true (the
/// residual error is dominated by tokenizer differences between models and
/// scripts) but not guaranteed. The measured
/// `measured_prompt_tokens` is from the ANCHOR request; `total_tokens` is
/// the CURRENT context's scaled estimate — they legitimately differ (the
/// history has grown since).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Calibration {
    /// The anchor request's provider-reported prompt size (input +
    /// cache-write + cache-read).
    pub measured_prompt_tokens: u64,
    /// measured ÷ estimated at the anchor; every component × scale.
    pub scale: f64,
}

/// Units in the byte prefix `s[..byte_end]` (the end need not be a char
/// boundary: a straddling character is attributed to the earlier segment).
/// The segment offsets come from byte lengths of whole string prefixes, so in
/// practice the boundaries are clean; this stays panic-free if a hand-built
/// `ContextBreakdown` input disagrees.
fn units_upto(s: &str, byte_end: usize) -> usize {
    s.char_indices()
        .take_while(|(i, _)| *i < byte_end)
        .map(|(_, c)| char_units(c))
        .sum()
}

/// Build the breakdown. Pure function over the agent's parts (no `&Agent`
/// dependency, so it unit-tests without one):
/// - `system` + `env_header_chars` + `skill_intro_base_chars`: the three-way
///   split of the system prompt (`env` includes its separator; everything
///   after `env + skill_intro_base_chars` bytes is the skill intro).
/// - `tools` / `tool_defs`: positionally aligned (the agent builds and
///   rebuilds them together — `try_new` / `refresh_tool_defs`).
/// - `calibration`: `Some` scales every component uniformly.
pub(crate) fn build(
    system: Option<&str>,
    env_header_chars: usize,
    skill_intro_base_chars: usize,
    tools: &[Box<dyn Tool>],
    tool_defs: &[ToolDef],
    messages: &[Message],
    calibration: Option<Calibration>,
) -> ContextBreakdown {
    debug_assert_eq!(
        tools.len(),
        tool_defs.len(),
        "tools and tool_defs must be positionally aligned"
    );

    // One running-floor allocator over ALL chars segments in a fixed order
    // (system parts, then tools in list order, then blocks in history
    // order): each segment gets its cumulative-quotient share of the single
    // ÷CHARS_PER_TOKEN division, so the parts sum to the whole exactly (see
    // the module docs' sum invariant).
    let mut acc = 0usize;
    let mut prev_q = 0usize;
    fn alloc(chars: usize, acc: &mut usize, prev_q: &mut usize) -> u64 {
        *acc = acc.saturating_add(chars);
        let q = *acc / CHARS_PER_TOKEN;
        let share = q - *prev_q;
        *prev_q = q;
        share as u64
    }

    // ── System prompt: three segments ─────────────────────────────────────
    // Kept as its own binding: `system` below is shadowed by the breakdown
    // struct, and the uncalibrated sum assert at the end needs the original.
    // The byte offsets locate the segments; the ALLOCATED units are CJK-aware
    // (`text_units`), matching `compress::estimate_tokens`'s token channel.
    let system_text = system;
    let sys_len = system.map_or(0, str::len);
    let env_len = env_header_chars.min(sys_len);
    let base_len = skill_intro_base_chars.min(sys_len - env_len);
    let sys_units = system.map(text_units).unwrap_or(0);
    let env_units = system.map(|s| units_upto(s, env_len)).unwrap_or(0);
    let base_units = system
        .map(|s| units_upto(s, env_len + base_len).saturating_sub(env_units))
        .unwrap_or(0);
    let intro_units = sys_units
        .saturating_sub(env_units)
        .saturating_sub(base_units);
    let mut system = SystemBreakdown {
        env: alloc(env_units, &mut acc, &mut prev_q),
        base: alloc(base_units, &mut acc, &mut prev_q),
        skill_intro: alloc(intro_units, &mut acc, &mut prev_q),
        tokens: 0,
    };
    system.tokens = system
        .env
        .saturating_add(system.base)
        .saturating_add(system.skill_intro);
    // Invariant: the three segments are the FIRST allocations in the single
    // running-floor division, so their sum is exactly floor(sys_units/4) — the
    // split exhausts the whole prompt with no remainder and no double count.
    debug_assert_eq!(
        system.tokens,
        (sys_units / CHARS_PER_TOKEN) as u64,
        "system segments must sum to the whole prompt's floor share"
    );

    // ── Tools: one segment per tool, bucketed by origin ───────────────────
    let mut tools_bd = ToolsBreakdown::default();
    for (t, def) in tools.iter().zip(tool_defs) {
        let chars = def.name.len() + def.description.len() + def.schema.to_string().len();
        let tokens = alloc(chars, &mut acc, &mut prev_q);
        let entry = ToolEntry {
            name: def.name.clone(),
            tokens,
        };
        let bucket = match t.origin() {
            ToolOrigin::Builtin => &mut tools_bd.builtin,
            ToolOrigin::Mcp => &mut tools_bd.mcp,
            ToolOrigin::Skill => &mut tools_bd.skill,
            ToolOrigin::Custom => &mut tools_bd.custom,
        };
        bucket.tokens = bucket.tokens.saturating_add(tokens);
        bucket.tools.push(entry);
    }
    tools_bd.tokens = [
        tools_bd.builtin.tokens,
        tools_bd.mcp.tokens,
        tools_bd.skill.tokens,
        tools_bd.custom.tokens,
    ]
    .into_iter()
    .fold(0u64, u64::saturating_add);
    // Invariant: each origin bucket's total is exactly its per-tool entries
    // summed — this loop keeps two representations in sync (bucket.tokens and
    // the pushed entry share one `tokens` value), pinned so a future edit
    // cannot count a tool in one and not the other.
    debug_assert!(
        [
            &tools_bd.builtin,
            &tools_bd.mcp,
            &tools_bd.skill,
            &tools_bd.custom
        ]
        .into_iter()
        .all(|c| c.tokens == c.tools.iter().map(|t| t.tokens).sum::<u64>()),
        "each origin bucket must equal the sum of its per-tool entries"
    );

    // ── Messages: one or two segments per block, bucketed by kind ─────────
    // ToolResult blocks split in two (text content vs image payload) so the
    // payload lands in the image bucket. PDF documents get their page-aware
    // add-on straight into the document bucket (no floor involvement).
    let mut kinds = MessageKinds::default();
    // tool_use_id → tool name, to attribute results (a ToolResult block
    // carries only the id). Same pattern as compress's `todo_use_ids`.
    let mut use_names: std::collections::HashMap<&str, &str> =
        std::collections::HashMap::with_capacity(messages.len());
    for m in messages {
        for b in &m.content {
            if let Block::ToolUse { id, name, .. } = b {
                use_names.insert(id.as_str(), name.as_str());
            }
        }
    }
    let mut messages_tokens = 0u64;
    let add = |bucket: &mut u64, tokens: u64, total: &mut u64| {
        *bucket = bucket.saturating_add(tokens);
        *total = total.saturating_add(tokens);
    };
    for m in messages {
        for b in &m.content {
            match b {
                Block::Text(t) => {
                    let tok = alloc(text_units(t), &mut acc, &mut prev_q);
                    match m.role {
                        Role::User => add(&mut kinds.user_text, tok, &mut messages_tokens),
                        Role::Assistant => {
                            add(&mut kinds.assistant_text, tok, &mut messages_tokens)
                        }
                    };
                }
                Block::Image { .. } => {
                    let tok = alloc(block_chars(b), &mut acc, &mut prev_q);
                    add(&mut kinds.image, tok, &mut messages_tokens);
                }
                Block::Document { .. } => {
                    let tok = alloc(block_chars(b), &mut acc, &mut prev_q);
                    let pdf = block_pdf_tokens(b) as u64;
                    add(&mut kinds.document, tok, &mut messages_tokens);
                    // The page add-on bypasses the floor channel; it is part
                    // of the document bucket AND the totals so the sum
                    // invariant still closes against estimate_tokens.
                    add(&mut kinds.document, pdf, &mut messages_tokens);
                }
                Block::ToolUse { name, .. } => {
                    let tok = alloc(block_chars(b), &mut acc, &mut prev_q);
                    if name == "read_skill" {
                        add(&mut kinds.skill_content, tok, &mut messages_tokens);
                    } else {
                        add(&mut kinds.tool_use, tok, &mut messages_tokens);
                    }
                }
                Block::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    let is_skill = use_names.get(tool_use_id.as_str()) == Some(&"read_skill");
                    let tok = alloc(text_units(content), &mut acc, &mut prev_q);
                    if is_skill {
                        add(&mut kinds.skill_content, tok, &mut messages_tokens);
                    } else {
                        add(&mut kinds.tool_result, tok, &mut messages_tokens);
                    }
                    if let Some(payload) = b_image_payload_chars(b) {
                        let tok = alloc(payload, &mut acc, &mut prev_q);
                        add(&mut kinds.image, tok, &mut messages_tokens);
                    }
                } // Exhaustive on purpose (same discipline as `block_chars`):
                  // within the defining crate `#[non_exhaustive]` doesn't apply,
                  // so a future `Block` variant fails THIS match at compile time
                  // and must pick a bucket — `other` is the explicit fallback
                  // destination, not an implicit wildcard.
            }
        }
    }

    // Invariant: by-kind attribution is a PARTITION of messages.tokens — the
    // `add` closure books every token to exactly one bucket and the running
    // total in the same step, so no token is double-counted across buckets
    // or silently dropped from the total.
    debug_assert_eq!(
        kinds.user_text
            + kinds.assistant_text
            + kinds.tool_use
            + kinds.tool_result
            + kinds.image
            + kinds.document
            + kinds.skill_content
            + kinds.other,
        messages_tokens,
        "by-kind buckets must sum exactly to messages.tokens"
    );

    let mut bd = ContextBreakdown {
        system,
        tools: tools_bd,
        messages: MessagesBreakdown {
            tokens: messages_tokens,
            by_kind: kinds,
        },
        total_tokens: 0,
        calibration,
    };
    bd.total_tokens = bd
        .system
        .tokens
        .saturating_add(bd.tools.tokens)
        .saturating_add(bd.messages.tokens);
    // Invariant (module docs / compress `block_chars`): UNCALIBRATED, the
    // whole breakdown closes exactly against the one independent estimate —
    // `compress::estimate_tokens` over the same inputs. Calibrated totals
    // legitimately drift from their parts by rounding (see `scale_breakdown`
    // docs), so the equality is only assertable on the None register.
    if calibration.is_none() {
        debug_assert_eq!(
            bd.total_tokens,
            estimate_tokens(
                messages,
                system_text,
                tool_defs
                    .iter()
                    .map(|d| d.name.len() + d.description.len() + d.schema.to_string().len())
                    .sum()
            ) as u64,
            "uncalibrated breakdown must equal compress::estimate_tokens"
        );
    }
    if let Some(c) = calibration {
        scale_breakdown(&mut bd, c.scale);
    }
    bd
}

/// Image payload chars of a ToolResult block, `None` when it carries none.
/// (Mirror of what `block_chars` folds in for the block; kept separate so
/// the payload can be allocated to the image bucket while the text content
/// goes to its own.)
fn b_image_payload_chars(b: &Block) -> Option<usize> {
    match b {
        Block::ToolResult { image, .. } => image.as_deref().map(image_payload_len),
        _ => None,
    }
}

/// Multiply every component by `scale` in place. Totals are scaled as
/// THEMSELVES (not recomputed from scaled parts), so a total stays the
/// scaled truth while its parts may drift from it by rounding — documented
/// behavior. The float→int cast saturates at `u64::MAX` and maps NaN to 0;
/// `scale` is constructed finite-positive, so those paths are unreachable
/// defensive behavior, not something callers must handle.
fn scale_breakdown(bd: &mut ContextBreakdown, scale: f64) {
    fn scale_u64(x: u64, s: f64) -> u64 {
        (x as f64 * s).round() as u64
    }
    bd.system.env = scale_u64(bd.system.env, scale);
    bd.system.base = scale_u64(bd.system.base, scale);
    bd.system.skill_intro = scale_u64(bd.system.skill_intro, scale);
    bd.system.tokens = scale_u64(bd.system.tokens, scale);
    for cat in [
        &mut bd.tools.builtin,
        &mut bd.tools.mcp,
        &mut bd.tools.skill,
        &mut bd.tools.custom,
    ] {
        cat.tokens = scale_u64(cat.tokens, scale);
        for t in &mut cat.tools {
            t.tokens = scale_u64(t.tokens, scale);
        }
    }
    bd.tools.tokens = scale_u64(bd.tools.tokens, scale);
    let k = &mut bd.messages.by_kind;
    for field in [
        &mut k.user_text,
        &mut k.assistant_text,
        &mut k.tool_use,
        &mut k.tool_result,
        &mut k.image,
        &mut k.document,
        &mut k.skill_content,
        &mut k.other,
    ] {
        *field = scale_u64(*field, scale);
    }
    bd.messages.tokens = scale_u64(bd.messages.tokens, scale);
    bd.total_tokens = scale_u64(bd.total_tokens, scale);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress::estimate_tokens;
    use crate::provider::{Message, ToolDef};
    use crate::tools::{Tool, ToolCtx, ToolOrigin, ToolOutput};
    use anyhow::Result;
    use serde_json::{json, Value};

    /// Configurable test tool: fixed def + origin override.
    struct FakeTool {
        def: ToolDef,
        origin: ToolOrigin,
    }
    impl Tool for FakeTool {
        fn def(&self) -> ToolDef {
            self.def.clone()
        }
        fn run(&self, _input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
            Ok(ToolOutput::text("ok"))
        }
        fn origin(&self) -> ToolOrigin {
            self.origin
        }
    }

    fn boxed(tools: Vec<FakeTool>) -> Vec<Box<dyn Tool>> {
        tools
            .into_iter()
            .map(|t| Box::new(t) as Box<dyn Tool>)
            .collect()
    }

    /// A tiny in-memory "PDF" whose page count the scanner can see:
    /// N uncompressed `/Type /Page` markers ⇒ N pages.
    fn pdf_b64(pages: usize) -> String {
        use base64::Engine;
        let mut raw = b"%PDF-1.4".to_vec();
        for _ in 0..pages {
            raw.extend_from_slice(b"\n/Type /Page");
        }
        base64::engine::general_purpose::STANDARD.encode(&raw)
    }

    fn mixed_history() -> Vec<Message> {
        vec![
            Message::user(vec![Block::Text("hello world 你好世界".into())]),
            Message::assistant(vec![
                Block::Text("assistant reply".into()),
                Block::ToolUse {
                    id: "t1".into(),
                    name: "read_skill".into(),
                    input: json!({"name": "run-code"}),
                },
                Block::ToolUse {
                    id: "t2".into(),
                    name: "list_skill".into(),
                    input: json!({}),
                },
            ]),
            Message::user(vec![
                Block::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "SKILL.md body (level 2 content)".into(),
                    image: None,
                    is_error: false,
                },
                Block::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "one skill listed".into(),
                    image: None,
                    is_error: false,
                },
            ]),
            Message::user(vec![
                Block::Image {
                    media_type: "image/png".into(),
                    data: "A".repeat(1000),
                },
                Block::ToolResult {
                    tool_use_id: "orphan".into(),
                    content: "result with no matching use".into(),
                    image: Some(Box::new(Block::Image {
                        media_type: "image/png".into(),
                        data: "B".repeat(400),
                    })),
                    is_error: false,
                },
                Block::Document {
                    media_type: "application/pdf".into(),
                    data: pdf_b64(3),
                },
            ]),
        ]
    }

    fn mixed_tools() -> (Vec<Box<dyn Tool>>, Vec<ToolDef>) {
        let tools = vec![
            FakeTool {
                def: ToolDef {
                    name: "shell".into(),
                    description: "d1".into(),
                    schema: json!({"type": "object"}),
                },
                origin: ToolOrigin::Builtin,
            },
            FakeTool {
                def: ToolDef {
                    name: "mcp_fetch".into(),
                    description: "d2".into(),
                    schema: json!({"type": "object"}),
                },
                origin: ToolOrigin::Mcp,
            },
            FakeTool {
                def: ToolDef {
                    name: "read_skill".into(),
                    description: "d3".into(),
                    schema: json!({"type": "object"}),
                },
                origin: ToolOrigin::Skill,
            },
            FakeTool {
                def: ToolDef {
                    name: "host_cb".into(),
                    description: "d4".into(),
                    schema: json!({"type": "object"}),
                },
                origin: ToolOrigin::Custom,
            },
        ];
        let defs = tools.iter().map(|t| t.def()).collect();
        (boxed(tools), defs)
    }

    /// THE invariant: uncalibrated, the breakdown's parts sum to exactly
    /// `estimate_tokens` over the same inputs — including CJK text, images,
    /// PDFs (page add-on), read_skill pairs, orphan results and images
    /// riding inside ToolResults.
    #[test]
    fn uncalibrated_total_equals_estimate_tokens() {
        let (tools, defs) = mixed_tools();
        let messages = mixed_history();
        let system = Some("# Environment\n- OS: macos\n\nYou are helpful.\n\n## Skills intro");
        // env = "# Environment\n- OS: macos\n\n" (28 chars), base = the rest
        // up to the intro separator.
        let bd = build(
            system,
            28,
            "You are helpful.".len(),
            &tools,
            &defs,
            &messages,
            None,
        );

        let tool_schema_chars: usize = defs
            .iter()
            .map(|t| t.name.len() + t.description.len() + t.schema.to_string().len())
            .sum();
        let expected = estimate_tokens(&messages, system, tool_schema_chars) as u64;
        assert_eq!(
            bd.system
                .tokens
                .saturating_add(bd.tools.tokens)
                .saturating_add(bd.messages.tokens),
            expected
        );
        assert_eq!(bd.total_tokens, expected);
        assert!(bd.calibration.is_none());
    }

    /// The three system segments partition the prompt exactly, each holding
    /// its running-floor share (env first, so base+intro together hold the
    /// floor difference across their combined char range).
    #[test]
    fn segments_partition_exactly() {
        let base = "You are helpful.";
        let intro = "\n\n## Available skills (skills)\n- run-code: do things";
        let env = "# Environment\n- OS: macos\n- Shell: sh -c\n";
        let system = format!("{env}\n\n{base}{intro}");
        let env_c = env.len() + 2; // env + its "\n\n" separator
        let bd = build(Some(&system), env_c, base.len(), &[], &[], &[], None);
        assert_eq!(
            bd.system.env + bd.system.base + bd.system.skill_intro,
            bd.system.tokens
        );
        assert_eq!(bd.system.env, (env_c / CHARS_PER_TOKEN) as u64);
        assert_eq!(
            bd.system.base + bd.system.skill_intro,
            ((system.len() / CHARS_PER_TOKEN) - (env_c / CHARS_PER_TOKEN)) as u64
        );
    }

    /// Four origin buckets each hold exactly their tools, in list order;
    /// with no system prompt the per-tool segments run first, so their sum
    /// is exactly the tool_schema_chars yardstick / 4.
    #[test]
    fn per_tool_and_category_sums() {
        let (tools, defs) = mixed_tools();
        let bd = build(None, 0, 0, &tools, &defs, &[], None);
        fn names(c: &ToolCategory) -> Vec<&str> {
            c.tools.iter().map(|t| t.name.as_str()).collect()
        }
        assert_eq!(names(&bd.tools.builtin), ["shell"]);
        assert_eq!(names(&bd.tools.mcp), ["mcp_fetch"]);
        assert_eq!(names(&bd.tools.skill), ["read_skill"]);
        assert_eq!(names(&bd.tools.custom), ["host_cb"]);
        // Category tokens == sum of the per-tool entries inside it.
        for cat in [
            &bd.tools.builtin,
            &bd.tools.mcp,
            &bd.tools.skill,
            &bd.tools.custom,
        ] {
            assert_eq!(cat.tokens, cat.tools.iter().map(|t| t.tokens).sum::<u64>());
        }
        assert_eq!(
            bd.tools.tokens,
            bd.tools.builtin.tokens
                + bd.tools.mcp.tokens
                + bd.tools.skill.tokens
                + bd.tools.custom.tokens
        );
        let per_tool_chars: usize = defs
            .iter()
            .map(|t| t.name.len() + t.description.len() + t.schema.to_string().len())
            .sum();
        assert_eq!(bd.tools.tokens, (per_tool_chars / CHARS_PER_TOKEN) as u64);
    }

    /// read_skill traffic (tool_use args AND the paired result body) lands
    /// in skill_content; list_skill stays ordinary tool traffic; an orphan
    /// result (no matching use) falls to tool_result. Minimal history, no
    /// system/tools, so each bucket holds exactly its contiguous char
    /// range's floor share.
    #[test]
    fn read_skill_attributed_to_skill_content() {
        let messages = vec![
            Message::assistant(vec![
                Block::ToolUse {
                    id: "t1".into(),
                    name: "read_skill".into(),
                    input: json!({"name": "run-code"}),
                },
                Block::ToolUse {
                    id: "t2".into(),
                    name: "list_skill".into(),
                    input: json!({}),
                },
            ]),
            Message::user(vec![
                Block::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "SKILL.md body (level 2 content)".into(),
                    image: None,
                    is_error: false,
                },
                Block::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "one skill listed".into(),
                    image: None,
                    is_error: false,
                },
                Block::ToolResult {
                    tool_use_id: "orphan".into(),
                    content: "result with no matching use".into(),
                    image: None,
                    is_error: false,
                },
            ]),
        ];
        let bd = build(None, 0, 0, &[], &[], &messages, None);

        let read_use = serde_json::to_string(&json!({"name": "run-code"}))
            .unwrap()
            .len();
        let skill_body = "SKILL.md body (level 2 content)".len();
        let list_use = serde_json::to_string(&json!({})).unwrap().len();
        let ordinary = "one skill listed".len() + "result with no matching use".len();
        // skill_content: the read_skill use runs first (floor-exact), then
        // its body; the two segments are contiguous ⇒ combined floor share.
        assert_eq!(
            bd.messages.by_kind.skill_content,
            ((read_use + skill_body) / 4) as u64
        );
        assert_eq!(bd.messages.by_kind.tool_result, (ordinary / 4) as u64);
        // tool_use holds ONLY the list_skill use's share. Its 2 chars inherit
        // the running floor's remainder from the 19-char read_use segment
        // before them (19 ≡ 3 mod 4 ⇒ the next multiple of 4 lands inside
        // it), so it is 1, not the naive 2/4 = 0 — derive it from the
        // invariant instead of hardcoding the floor dance.
        assert_eq!(
            bd.messages.by_kind.tool_use,
            bd.messages.tokens
                - bd.messages.by_kind.skill_content
                - bd.messages.by_kind.tool_result
        );
        assert!(bd.messages.by_kind.tool_use <= (list_use / 4) as u64 + 1);
    }

    #[test]
    fn calibration_scales_uniformly() {
        let (tools, defs) = mixed_tools();
        let messages = mixed_history();
        let unscaled = build(None, 0, 0, &tools, &defs, &messages, None);
        let scaled = build(
            None,
            0,
            0,
            &tools,
            &defs,
            &messages,
            Some(Calibration {
                measured_prompt_tokens: 2000,
                scale: 2.0,
            }),
        );
        // scale 2.0 is exact for integers — every field doubles.
        assert_eq!(scaled.system.tokens, unscaled.system.tokens * 2);
        assert_eq!(scaled.tools.tokens, unscaled.tools.tokens * 2);
        assert_eq!(scaled.messages.tokens, unscaled.messages.tokens * 2);
        assert_eq!(scaled.total_tokens, unscaled.total_tokens * 2);
        assert_eq!(
            scaled.messages.by_kind.image,
            unscaled.messages.by_kind.image * 2
        );
        let c = scaled.calibration.unwrap();
        assert_eq!(c.measured_prompt_tokens, 2000);
        assert_eq!(c.scale, 2.0);
    }

    /// An image riding inside a ToolResult counts as image (its chars
    /// split from the text content's bucket).
    #[test]
    fn toolresult_image_counts_as_image() {
        let (tools, defs) = mixed_tools();
        let messages = vec![Message::user(vec![Block::ToolResult {
            tool_use_id: "x".into(),
            content: "c".repeat(400),
            image: Some(Box::new(Block::Image {
                media_type: "image/png".into(),
                data: "D".repeat(800),
            })),
            is_error: false,
        }])];
        let bd = build(None, 0, 0, &tools, &defs, &messages, None);
        // 800 base64 chars → 8 chars-equivalent; 400 text chars → 100 tok.
        assert_eq!(bd.messages.by_kind.image, (800 / 100 / 4) as u64);
        assert_eq!(bd.messages.by_kind.tool_result, (400 / 4) as u64);
    }

    /// A multi-page PDF's page add-on lands in the document bucket on top
    /// of its chars share.
    #[test]
    fn document_counts_page_addon() {
        let (tools, defs) = mixed_tools();
        let data = pdf_b64(3);
        let messages = vec![Message::user(vec![Block::Document {
            media_type: "application/pdf".into(),
            data: data.clone(),
        }])];
        let bd = build(None, 0, 0, &tools, &defs, &messages, None);
        let pages = crate::compress::pdf_page_estimate(&data);
        assert_eq!(pages, 3);
        // chars share is sub-token (tiny pdf) → floor diff covers it; the
        // page add-on dominates and must be present in full.
        assert_eq!(bd.messages.by_kind.document, 3 * 3000);
    }

    /// Empty everything: all zeros, no panic, totals close at 0.
    #[test]
    fn empty_context_is_all_zero() {
        let bd = build(None, 0, 0, &[], &[], &[], None);
        assert_eq!(bd.total_tokens, 0);
        assert_eq!(bd.system.tokens, 0);
        assert_eq!(bd.tools.tokens, 0);
        assert_eq!(bd.messages.tokens, 0);
    }
}
