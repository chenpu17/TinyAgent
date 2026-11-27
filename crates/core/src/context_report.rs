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
    block_chars, block_pdf_tokens, char_units, estimate_tokens, image_payload_len, reasoning_units,
    text_units, CHARS_PER_TOKEN,
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
    /// Sum of the per-tool estimates below (running-floor allocated, so the
    /// four buckets sum to `ToolsBreakdown::tokens` exactly).
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
    /// The tool's name (agent-facing, post-sanitization for MCP tools).
    pub name: String,
    /// This tool's share of the schema estimate.
    pub tokens: u64,
}

/// Tool schema cost by [`ToolOrigin`]. The per-tool chars here sum to the
/// same total `Agent::tool_schema_chars` caches.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ToolsBreakdown {
    /// `ToolOrigin::Builtin` tools (shell / files / image / todo / task /
    /// run_code).
    pub builtin: ToolCategory,
    /// `ToolOrigin::Mcp` tools.
    pub mcp: ToolCategory,
    /// The `read_skill` / `list_skill` pair.
    pub skill: ToolCategory,
    /// Host-injected or unrecognized tools (`ToolOrigin::Custom`'s default).
    pub custom: ToolCategory,
    /// The four buckets summed.
    pub tokens: u64,
}

/// Advisory line for the ESTIMATED size of the tool table: more than this
/// many tokens of tool definitions. Every request carries the whole table, so
/// it is a standing cost no compaction can fold. A rule of thumb taken from
/// Anthropic's Tool search documentation — an empirical figure, not a
/// provider limit: crossing it advises, never acts.
pub const TOOL_DEFS_ADVISORY_TOKENS: u64 = 10_000;

/// Advisory line for the NUMBER of tool definitions (see
/// [`TOOL_DEFS_ADVISORY_TOKENS`]): more than this many tools. The two lines
/// are independent — either one alone is worth a notice.
pub const TOOL_DEFS_ADVISORY_COUNT: usize = 30;

/// Which advisory line(s) a tool table is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolDefsAdvisory {
    /// Over [`TOOL_DEFS_ADVISORY_TOKENS`] only.
    Tokens,
    /// Over [`TOOL_DEFS_ADVISORY_COUNT`] only.
    Count,
    /// Over both.
    Both,
}

impl ToolsBreakdown {
    /// Tool definitions across the four origin buckets.
    pub fn tool_count(&self) -> usize {
        self.builtin
            .tools
            .len()
            .saturating_add(self.mcp.tools.len())
            .saturating_add(self.skill.tools.len())
            .saturating_add(self.custom.tools.len())
    }

    /// The advisory line(s) this table is over — judged on `tokens` as
    /// reported (scaled when a calibration is live) and [`Self::tool_count`];
    /// `None` under both.
    pub fn advisory(&self) -> Option<ToolDefsAdvisory> {
        tool_defs_advisory(self.tokens, self.tool_count())
    }
}

/// Which advisory line(s) a tool table of `tokens` estimated tokens across
/// `count` definitions is over. Strictly "more than": a table exactly ON a
/// line is not over it.
pub fn tool_defs_advisory(tokens: u64, count: usize) -> Option<ToolDefsAdvisory> {
    match (
        tokens > TOOL_DEFS_ADVISORY_TOKENS,
        count > TOOL_DEFS_ADVISORY_COUNT,
    ) {
        (false, false) => None,
        (true, false) => Some(ToolDefsAdvisory::Tokens),
        (false, true) => Some(ToolDefsAdvisory::Count),
        (true, true) => Some(ToolDefsAdvisory::Both),
    }
}

/// The advisory as display lines — no tag and no indent, each surface adds
/// its own. `None` under both lines. ONE text for every surface (the
/// construction-time diag notice, the TUI's `/context`) so they cannot drift
/// apart. One statement per line, every line well under 80 columns: the
/// notice reaches a terminal through the stderr fallback, where a long line
/// is hard-wrapped by the terminal itself — mid-word, at whatever the width
/// happens to be.
pub fn tool_defs_advisory_lines(tokens: u64, count: usize) -> Option<Vec<String>> {
    let crossed = match tool_defs_advisory(tokens, count)? {
        ToolDefsAdvisory::Tokens => format!("the advisory line of {TOOL_DEFS_ADVISORY_TOKENS} tokens"),
        ToolDefsAdvisory::Count => format!("the advisory line of {TOOL_DEFS_ADVISORY_COUNT} tools"),
        ToolDefsAdvisory::Both => format!(
            "both advisory lines ({TOOL_DEFS_ADVISORY_TOKENS} tokens, {TOOL_DEFS_ADVISORY_COUNT} tools)"
        ),
    };
    let count_s = if count == 1 {
        "1 tool".to_owned()
    } else {
        format!("{count} tools")
    };
    Some(vec![
        format!("tool definitions ≈ {tokens} tokens across {count_s}."),
        format!("Over {crossed}."),
        "Fixed cost on every request; compaction cannot fold it.".to_owned(),
        "Trim tools / MCP servers, or move rarely used tools into skills.".to_owned(),
        "Rules of thumb from Anthropic's Tool search docs, on estimates.".to_owned(),
    ])
}

/// [`ToolsBreakdown::tokens`] for a table of `tool_units` units behind a
/// system prompt — exactly the number an uncalibrated `build` reports, at the
/// cost of one pass over the system prompt instead of another over the table.
/// The allocator books the system prompt's three segments first (together
/// exactly `floor(sys / CHARS_PER_TOKEN)`) and then one segment per tool, so
/// the running floor over the tools telescopes to the difference of two
/// quotients. Pinned against `build` in the tests.
pub(crate) fn tool_table_tokens(system: Option<&str>, tool_units: usize) -> u64 {
    let sys = system.map_or(0, text_units);
    let after = sys.saturating_add(tool_units) / CHARS_PER_TOKEN;
    (after - sys / CHARS_PER_TOKEN) as u64
}

/// The construction-time notice through the diag channel (`[context] …`,
/// warn level) — once per process per distinct text. A batch run builds one
/// agent per task from the same table and a long-lived host rebuilds
/// sessions: the first crossing is the news, the repeats are noise. The
/// notice is a snapshot of one table; [`ToolsBreakdown::advisory`] stays the
/// live answer for hosts that need the current one. One statement per line,
/// continuation lines indented two spaces (a sink that renders messages as
/// rows receives the same text and adds its own continuation pad).
pub(crate) fn note_tool_defs_advisory(tokens: u64, count: usize) {
    let Some(lines) = tool_defs_advisory_lines(tokens, count) else {
        return;
    };
    let text = format!("[context] {}", lines.join("\n  "));
    static SAID: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeSet<String>>> =
        std::sync::OnceLock::new();
    let said = SAID.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeSet::new()));
    let fresh = said
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(text.clone());
    if fresh {
        crate::diag::diag_warn!("{text}");
    }
}

/// Message history aggregated by block kind.
#[derive(Debug, Clone, Serialize)]
pub struct MessagesBreakdown {
    /// Whole conversation history's estimate (the `by_kind` numbers sum to
    /// this by the same running-floor allocation as the other sections).
    pub tokens: u64,
    /// The same total split by block kind.
    pub by_kind: MessageKinds,
}

/// Message-history kinds. There is no `Block`-level `thinking` bucket:
/// thinking streams as a pure event feed (`ThinkingDelta`) and the `Block`
/// enum has no variant for it. What DOES enter history is the provider's
/// carried reasoning state (`Message::reasoning`, kept only while
/// `Config.preserved_reasoning` is on) — that has its own `reasoning` bucket
/// below, and is 0 (like every bucket here) whenever nothing carries it. If a
/// `Block::Thinking` variant ever lands, the classifier's exhaustive match
/// fails at compile time and its author places it in `other` first, promoting
/// it to a named field when warranted.
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
    /// Carried provider reasoning state (`Message.reasoning` — thinking text
    /// plus its opaque signature/redaction payload). It rides on every
    /// request until its turn ends, so it is context the model pays for.
    pub reasoning: u64,
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
        // Same CJK-aware units yardstick as Agent's tool_schema_chars cache
        // (compress::tool_schema_units, round-4 R4D-F1) — the bucket total
        // must stay equal to the cached sum the estimate gate uses.
        let units = crate::compress::tool_schema_units(
            &def.name,
            &def.description,
            &def.schema.to_string(),
        );
        let tokens = alloc(units, &mut acc, &mut prev_q);
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
        // Carried reasoning rides beside the blocks (a side field, not a
        // Block) — one segment per message, after its blocks, same running
        // floor so the sum invariant below still closes.
        let carried = reasoning_units(m);
        if carried > 0 {
            let tok = alloc(carried, &mut acc, &mut prev_q);
            add(&mut kinds.reasoning, tok, &mut messages_tokens);
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
            + kinds.reasoning
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
                    .map(|d| {
                        crate::compress::tool_schema_units(
                            &d.name,
                            &d.description,
                            &d.schema.to_string(),
                        )
                    })
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
        &mut k.reasoning,
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
        // The assistant turn also carries reasoning state (text + opaque
        // signature), so the sum invariant below covers that channel too.
        let mut assistant = Message::assistant(vec![
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
        ]);
        assistant.reasoning = Some(crate::provider::Reasoning {
            provider: "anthropic".into(),
            blocks: vec![crate::provider::ReasoningBlock {
                before: 0,
                text: "thinking it over 想一想".into(),
                opaque: json!({"signature": "sig=="}),
            }],
        });
        vec![
            Message::user(vec![Block::Text("hello world 你好世界".into())]),
            assistant,
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
            .map(|t| {
                crate::compress::tool_schema_units(&t.name, &t.description, &t.schema.to_string())
            })
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

    /// An assistant message carrying reasoning state — text + opaque vendor
    /// payload — beside one text block.
    fn history_with_reasoning() -> Vec<Message> {
        let mut assistant = Message::assistant(vec![Block::Text("a".repeat(400))]);
        assistant.reasoning = Some(crate::provider::Reasoning {
            provider: "anthropic".into(),
            blocks: vec![
                crate::provider::ReasoningBlock {
                    before: 0,
                    text: "t".repeat(800),
                    opaque: json!({"signature": "s".repeat(40)}),
                },
                crate::provider::ReasoningBlock {
                    before: 1,
                    text: String::new(),
                    opaque: json!({"data": "d".repeat(20)}),
                },
            ],
        });
        vec![Message::user(vec![Block::Text("q".into())]), assistant]
    }

    /// Carried reasoning (`Message::reasoning`) is its own bucket — readable
    /// text and opaque payload both, summed over the blocks — apart from the
    /// assistant text it rode with, and still inside the partition + sum
    /// invariants (it rides on every request of its turn, so it is context
    /// the model pays for).
    #[test]
    fn carried_reasoning_gets_its_own_bucket() {
        let (tools, defs) = mixed_tools();
        let messages = history_with_reasoning();
        let bd = build(None, 0, 0, &tools, &defs, &messages, None);
        // The opaque payloads count as their JSON form (what rides on the
        // wire): `{"signature":"s"*40}` is 56 chars, `{"data":"d"*20}` is
        // 31 → 800 + 56 + 31 = 887 units for reasoning, 400 for the
        // assistant text; the user's "q" is sub-token and lands in the
        // running floor.
        let reasoning_units = 800 + 56 + 31;
        assert_eq!(bd.messages.by_kind.reasoning, (reasoning_units / 4) as u64);
        assert_eq!(bd.messages.by_kind.assistant_text, (400 / 4) as u64);
        let k = &bd.messages.by_kind;
        assert_eq!(
            k.user_text
                + k.assistant_text
                + k.tool_use
                + k.tool_result
                + k.image
                + k.document
                + k.skill_content
                + k.reasoning
                + k.other,
            bd.messages.tokens,
            "the bucket is part of the partition, never an extra"
        );
        let tool_schema_chars: usize = defs
            .iter()
            .map(|t| {
                crate::compress::tool_schema_units(&t.name, &t.description, &t.schema.to_string())
            })
            .sum();
        assert_eq!(
            bd.total_tokens,
            estimate_tokens(&messages, None, tool_schema_chars) as u64
        );
        // Strip it and the bucket closes at zero, the rest unchanged — the
        // shape every history has while the carry switch is off.
        let mut bare = history_with_reasoning();
        crate::compress::strip_reasoning(&mut bare);
        let stripped = build(None, 0, 0, &tools, &defs, &bare, None);
        assert_eq!(stripped.messages.by_kind.reasoning, 0);
        assert_eq!(
            bd.messages.tokens.saturating_sub(stripped.messages.tokens),
            (reasoning_units / 4) as u64
        );
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
        assert_eq!(bd.tools.tool_count(), 0);
        assert_eq!(bd.tools.advisory(), None);
    }

    /// `tool_count` sums the four origin buckets; the advisory follows the
    /// numbers of the breakdown as reported — tokens and count, each line on
    /// its own, a table exactly ON a line is not over it.
    #[test]
    fn tools_breakdown_advisory_follows_the_table() {
        fn table(n: usize, desc_len: usize, calibration: Option<Calibration>) -> ToolsBreakdown {
            let tools: Vec<FakeTool> = (0..n)
                .map(|i| FakeTool {
                    def: ToolDef {
                        name: format!("t{i}"),
                        description: "d".repeat(desc_len),
                        schema: json!({"type": "object"}),
                    },
                    origin: ToolOrigin::Mcp,
                })
                .collect();
            let defs: Vec<ToolDef> = tools.iter().map(|t| t.def()).collect();
            build(None, 0, 0, &boxed(tools), &defs, &[], calibration).tools
        }
        let wide = 4 * TOOL_DEFS_ADVISORY_TOKENS as usize + 64;
        // Two small tools: under both lines.
        let small = table(2, 10, None);
        assert_eq!(small.tool_count(), 2);
        assert_eq!(small.advisory(), None);
        // 31 tools under the token line: the count line alone.
        let many = table(31, 1_200, None);
        assert_eq!(many.tool_count(), 31);
        assert!(
            many.tokens <= TOOL_DEFS_ADVISORY_TOKENS,
            "fixture drifted: {}",
            many.tokens
        );
        assert_eq!(many.advisory(), Some(ToolDefsAdvisory::Count));
        // One tool whose description alone crosses the token line.
        let one = table(1, wide, None);
        assert_eq!(one.tool_count(), 1);
        assert!(one.tokens > TOOL_DEFS_ADVISORY_TOKENS);
        assert_eq!(one.advisory(), Some(ToolDefsAdvisory::Tokens));
        assert_eq!(
            table(31, wide, None).advisory(),
            Some(ToolDefsAdvisory::Both)
        );
        // Judged on the numbers AS REPORTED: a live calibration scales the
        // table's tokens, so a table under the line uncalibrated can cross
        // it once the anchor says the estimate ran low.
        let scaled = table(
            2,
            1_200,
            Some(Calibration {
                measured_prompt_tokens: 90_000,
                scale: 32.0,
            }),
        );
        assert!(scaled.tokens > TOOL_DEFS_ADVISORY_TOKENS);
        assert_eq!(scaled.advisory(), Some(ToolDefsAdvisory::Tokens));
        assert_eq!(scaled.tool_count(), 2, "the count never scales");
    }

    /// The construction-time notice prints [`tool_table_tokens`]; `/context`
    /// prints `ToolsBreakdown::tokens`. One number for both — whatever the
    /// system prompt's and the table's units do to the running floor (the
    /// table is booked after the system prompt and before the history, so
    /// neither the split inside the table nor the history can move it).
    #[test]
    fn tool_table_tokens_is_the_breakdowns_number() {
        fn table() -> Vec<FakeTool> {
            (0..9)
                .map(|i| FakeTool {
                    def: ToolDef {
                        name: format!("t{i}"),
                        description: "d".repeat(37 * i + 5),
                        schema: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
                    },
                    origin: ToolOrigin::Mcp,
                })
                .collect()
        }
        let defs: Vec<ToolDef> = table().iter().map(|t| t.def()).collect();
        let units: usize = defs
            .iter()
            .map(|d| {
                crate::compress::tool_schema_units(&d.name, &d.description, &d.schema.to_string())
            })
            .sum();
        // (system prompt, env header bytes, base bytes)
        let prompts: [(Option<&str>, usize, usize); 9] = [
            (None, 0, 0),
            (Some(""), 0, 0),
            (Some("abc"), 0, 0),        // remainder 3
            (Some("abcd"), 0, 0),       // aligned
            (Some("a"), 0, 0),          // remainder 1
            (Some("ab"), 0, 0),         // remainder 2
            (Some("中文"), 0, 0),       // CJK: 18 units
            (Some("env\n\nbas"), 3, 5), // env / base / intro split
            (Some(&"x".repeat(1_001)), 0, 0),
        ];
        let own = (units / CHARS_PER_TOKEN) as u64;
        let mut seen = std::collections::BTreeSet::new();
        for (system, env, base) in prompts {
            let label = format!(
                "env {env} base {base} ({} units)",
                system.map_or(0, text_units)
            );
            let empty = build(system, env, base, &boxed(table()), &defs, &[], None);
            assert_eq!(
                empty.tools.tokens,
                tool_table_tokens(system, units),
                "{label}"
            );
            // The history is booked after the table: it cannot move the number.
            let full = build(
                system,
                env,
                base,
                &boxed(table()),
                &defs,
                &mixed_history(),
                None,
            );
            assert!(full.messages.tokens > 0, "fixture: a non-empty history");
            assert_eq!(
                full.tools.tokens, empty.tools.tokens,
                "{label}: history moved the table's number"
            );
            assert_eq!(full.tools.tool_count(), empty.tools.tool_count(), "{label}");
            seen.insert(tool_table_tokens(system, units));
        }
        // The whole point of the helper: the table's own floor is NOT always
        // the breakdown's number — the system prompt's remainder can hand
        // the table one more token.
        assert_eq!(
            seen.into_iter().collect::<Vec<_>>(),
            vec![own, own + 1],
            "fixture drifted: want both the table's own floor and one more"
        );
    }

    /// The advisory predicate is strictly "more than", per line: exactly on a
    /// line is not over it.
    #[test]
    fn tool_defs_advisory_is_strict_per_line() {
        assert_eq!(
            tool_defs_advisory(TOOL_DEFS_ADVISORY_TOKENS, TOOL_DEFS_ADVISORY_COUNT),
            None
        );
        assert_eq!(tool_defs_advisory(0, 0), None);
        assert_eq!(
            tool_defs_advisory(TOOL_DEFS_ADVISORY_TOKENS + 1, TOOL_DEFS_ADVISORY_COUNT),
            Some(ToolDefsAdvisory::Tokens)
        );
        assert_eq!(
            tool_defs_advisory(TOOL_DEFS_ADVISORY_TOKENS, TOOL_DEFS_ADVISORY_COUNT + 1),
            Some(ToolDefsAdvisory::Count)
        );
        assert_eq!(
            tool_defs_advisory(u64::MAX, usize::MAX),
            Some(ToolDefsAdvisory::Both)
        );
    }

    /// The display text names the line(s) actually crossed (never the one
    /// the table is under), says what to do, and labels the thresholds as
    /// rules of thumb on estimates. Nothing under both lines.
    #[test]
    fn tool_defs_advisory_lines_name_the_crossed_lines() {
        assert_eq!(tool_defs_advisory_lines(1, 1), None);
        assert_eq!(
            tool_defs_advisory_lines(TOOL_DEFS_ADVISORY_TOKENS, TOOL_DEFS_ADVISORY_COUNT),
            None
        );
        let count = tool_defs_advisory_lines(40, 31).unwrap();
        assert_eq!(
            count[0], "tool definitions ≈ 40 tokens across 31 tools.",
            "{count:?}"
        );
        assert_eq!(count[1], "Over the advisory line of 30 tools.", "{count:?}");
        assert!(
            !count[1].contains("tokens,"),
            "count-only text must not cite the token line: {:?}",
            count[1]
        );
        let tokens = tool_defs_advisory_lines(10_001, 3).unwrap();
        assert_eq!(tokens[0], "tool definitions ≈ 10001 tokens across 3 tools.");
        assert_eq!(tokens[1], "Over the advisory line of 10000 tokens.");
        let both = tool_defs_advisory_lines(10_001, 31).unwrap();
        assert_eq!(both[0], "tool definitions ≈ 10001 tokens across 31 tools.");
        assert_eq!(
            both[1],
            "Over both advisory lines (10000 tokens, 30 tools)."
        );
        // Singular.
        assert_eq!(
            tool_defs_advisory_lines(10_001, 1).unwrap()[0],
            "tool definitions ≈ 10001 tokens across 1 tool."
        );
        // Same guidance after the first two lines on every path.
        assert_eq!(both[2..], count[2..]);
        assert_eq!(both[2..], tokens[2..]);
        assert_eq!(
            both[2..].iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "Fixed cost on every request; compaction cannot fold it.",
                "Trim tools / MCP servers, or move rarely used tools into skills.",
                "Rules of thumb from Anthropic's Tool search docs, on estimates.",
            ]
        );
        assert_eq!(both.len(), 5);
        // Each line fits an 80-column terminal under the notice's own
        // prefix ("[context] " on the first, two spaces on the rest) with a
        // margin for wider numbers — the stderr fallback is hard-wrapped by
        // the terminal itself, mid-word, at whatever the width happens to be.
        for (i, line) in both.iter().enumerate() {
            let prefix = if i == 0 { "[context] " } else { "  " };
            assert!(
                prefix.len() + line.chars().count() <= 78,
                "{} cols: {prefix}{line}",
                prefix.len() + line.chars().count()
            );
            assert!(
                !line.ends_with(' '),
                "no trailing space for a terminal to wrap on: {line:?}"
            );
        }
    }
}
