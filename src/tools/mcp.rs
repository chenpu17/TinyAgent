use super::{Tool, ToolCtx, ToolOutput};
use crate::config::McpServerConfig;
use crate::mcp::McpClient;
use crate::provider::{Block, ToolDef};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// An MCP tool exposed to the agent as `mcp_<server>_<tool>`.
///
/// `Clone` is cheap and intentional: the `client` is an `Arc`, so cloning an
/// `McpTool` shares the underlying MCP connection rather than duplicating it.
/// The `task` sub-agent relies on this — it clones the parent's MCP tools into
/// each child instead of re-running `McpClient::start` (re-spawning every MCP
/// server per sub-agent call).
#[derive(Clone)]
pub struct McpTool {
    client: Arc<Mutex<McpClient>>,
    /// The owning server's CONFIG name (unsanitized) — what
    /// `Agent::remove_mcp_server` routes by. Distinct from the sanitized
    /// display name in `def` and from the wire name.
    server: String,
    wire_name: String,
    def: ToolDef,
}

/// Cap on a base64 image payload from an MCP tool result, mirroring
/// load_image's 10 MiB (raw) ≈ 14 MB base64. The text half of a result is
/// truncated to max_output_chars, but an unbounded image would sail straight
/// into the history and get re-sent on every request — a buggy or hostile
/// server could otherwise exhaust memory and the context window.
const MAX_MCP_IMAGE_B64: usize = 14 * 1024 * 1024;

impl Tool for McpTool {
    /// Guest-registry opt-in (run_code's `tools.call` surface). CRITICAL:
    /// the AppWorld-style play (data behind app APIs, guest loops pages
    /// in-sandbox) lives on this — dropping the override removes every
    /// `mcp_*` tool from the sandbox and breaks the whole mechanism.
    fn available_to_code(&self) -> bool {
        true
    }

    fn def(&self) -> ToolDef {
        self.def.clone()
    }

    /// The CONFIG name of the owning MCP server — what
    /// `Agent::remove_mcp_server(name)` routes by. `None` for every non-MCP
    /// tool (the trait default), so the agent never parses display names.
    fn mcp_server(&self) -> Option<&str> {
        Some(&self.server)
    }

    /// Lock-ordering note (#13 of the 2026-09 review): `run` and
    /// `set_shutdown` share the client mutex, and `run` holds it for the
    /// WHOLE call (request → sliced response wait). That means `set_shutdown`
    /// (session end) can briefly block behind an in-flight call — which is
    /// fine BY CONSTRUCTION: arming only happens at teardown, when the
    /// frontend has already stopped issuing calls, and the one race window
    /// (a call still in flight at quit) is closed by the run-level cancel —
    /// the wait is sliced at 50ms, so the flag releases the lock long before
    /// teardown's bounded join gives up. Do NOT "fix" this by taking a
    /// finer-grained lock: serializing requests whole is what keeps the
    /// stdio framing (one writer, id-matched reader) sound.
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        // Tolerate a poisoned lock: a panic while holding it leaves the client
        // state consistent (requests are serialized whole), and panicking here
        // would abort the process outright (release is panic="abort").
        // `ctx.cancel` rides along so the response wait polls the run's
        // cancel flag (50ms slices) — see `recv_cancelable`.
        let result = self.client.lock().unwrap_or_else(|e| e.into_inner()).call(
            "tools/call",
            json!({"name": self.wire_name, "arguments": input}),
            ctx.cancel.as_deref(),
        )?;

        let mut text = String::new();
        let mut image = None;
        let mut extra_images = 0u32;
        let mut oversized_images = 0u32;
        for c in result["content"].as_array().cloned().unwrap_or_default() {
            match c["type"].as_str().unwrap_or("") {
                "text" => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(c["text"].as_str().unwrap_or_default());
                }
                "image" => {
                    let data = c["data"].as_str().unwrap_or_default();
                    if data.len() > MAX_MCP_IMAGE_B64 {
                        oversized_images += 1;
                    // ToolOutput carries a single image; keep the first and
                    // note the rest rather than silently dropping them.
                    } else if image.is_none() {
                        image = Some(Block::Image {
                            media_type: c["mimeType"].as_str().unwrap_or("image/png").to_string(),
                            data: data.to_string(),
                        });
                    } else {
                        extra_images += 1;
                    }
                }
                _ => {}
            }
        }
        for note in [
            (extra_images > 0).then(|| format!("[{extra_images} additional image(s) omitted]")),
            (oversized_images > 0)
                .then(|| format!("[{oversized_images} image(s) dropped: over size limit]")),
        ]
        .into_iter()
        .flatten()
        {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&note);
        }
        if text.is_empty() && image.is_none() {
            text = "[empty tool result]".into();
        }
        // Truncate like shell/read_file/list_dir: an MCP tool returning
        // megabytes of text would otherwise bypass max_output_chars and bloat
        // the context window (this was the one tool that ignored the cap).
        crate::tools::truncate(&mut text, ctx.max_output_chars);
        Ok(ToolOutput {
            content: text,
            image,
            is_error: result["isError"].as_bool().unwrap_or(false),
            usage: None,
            meta: None,
        })
    }

    /// Forward the session-end shutdown flag to the shared MCP client, which
    /// arms shutdown-aware teardown on the transport (a stdio server's child is
    /// killed on shutdown, unblocking a stuck call so the agent can drop). This
    /// is the **session-end** signal, NOT the per-turn cancel — the latter is
    /// recoverable and must leave the server alive for the next turn, so it
    /// never reaches the reaper. All McpTools sharing this client arm the same
    /// reaper idempotently.
    fn set_shutdown(&mut self, shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.client
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_shutdown(shutdown);
    }
}

/// Start one MCP server and wrap its enumerated tools. Shared by `build`
/// (startup) and `build_one` (hot-add); returns concrete tools holding one
/// shared `Arc`'d client for the whole server.
fn start_server(cfg: &McpServerConfig) -> Result<Vec<McpTool>> {
    let (client, infos) = McpClient::start(cfg)?;
    let client = Arc::new(Mutex::new(client));
    let mut out = Vec::with_capacity(infos.len());
    for info in infos {
        let display = sanitize_name(&cfg.name, &info.name);
        out.push(McpTool {
            client: client.clone(),
            // Config name, unsanitized — remove_mcp_server routes by it.
            server: cfg.name.clone(),
            def: ToolDef {
                name: display,
                description: info.description,
                schema: if info.input_schema.is_null() {
                    json!({"type": "object", "properties": {}})
                } else {
                    info.input_schema
                },
            },
            // The MCP *wire* name is the original, unsanitized tool name —
            // tools/call must dispatch with the name the server knows.
            // Only the name shown to the model is sanitized.
            wire_name: info.name,
        });
    }
    Ok(out)
}

/// Start every configured MCP server and collect their tools.
/// A server that fails the handshake aborts startup with a clear error.
///
/// Returns concrete `McpTool`s (not boxed) so the parent agent's tool list and
/// the `task` sub-agent can SHARE them: the parent boxes its copies, while each
/// sub-agent `clone()`s the same set into its child (sharing the `Arc`'d MCP
/// connection) instead of re-calling this — re-spawning every server per task.
pub(crate) fn build(servers: &[McpServerConfig]) -> Result<Vec<McpTool>> {
    let mut out: Vec<McpTool> = Vec::new();
    let mut display_names: Vec<String> = Vec::new();
    for cfg in servers {
        let tools = start_server(cfg)?;
        let n = tools.len();
        display_names.extend(tools.iter().map(|t| t.def.name.clone()));
        out.extend(tools);
        eprintln!("mcp: server '{}' ready ({} tools)", cfg.name, n);
    }
    // Cross-server collisions after sanitization are the real risk — e.g.
    // server "a" + tool "b.c" and server "a.b" + tool "c" both yield
    // `mcp_a_b_c`. Surface it at startup rather than silently shadowing a tool.
    dedup_check(&display_names)?;
    Ok(out)
}

/// Start ONE MCP server for `Agent::add_mcp_server` (hot-add). Unlike `build`
/// this never prints — the agent layer can sit behind a raw-mode TUI, where an
/// eprintln corrupts the screen. `existing` holds every display name the agent
/// already exposes (built-in, MCP, skill-injected, host-registered); a
/// collision is a recoverable `Err`, and the `?` on that path drops the fresh
/// tool set, killing the just-spawned child via the Drop chain — a failed
/// hot-add leaves no orphan server process.
pub(crate) fn build_one(cfg: &McpServerConfig, existing: &[String]) -> Result<Vec<McpTool>> {
    let tools = start_server(cfg)?;
    let names: Vec<String> = tools.iter().map(|t| t.def.name.clone()).collect();
    check_new_names(&names, existing)?;
    Ok(tools)
}

/// Build the agent-facing tool name `mcp_<server>_<tool>`, sanitized so the
/// model can emit it reliably: characters outside `[A-Za-z0-9_]` become `_`,
/// runs of separators collapse, and leading/trailing separators are trimmed
/// (matching the ASCII-identifier convention from the MCP spec). The result is
/// capped at 64 chars (the Anthropic tool-name / OpenAI function-name limit);
/// the wire name is untouched — only this display name is sanitized, so
/// dispatch is safe. Two names that collide after sanitize+truncate are caught
/// by `dedup_check`.
fn sanitize_name(server: &str, tool: &str) -> String {
    fn clean(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut just_sep = true; // suppress a leading separator + collapse runs
        for c in s.chars() {
            if c.is_ascii_alphanumeric() {
                out.push(c);
                just_sep = false;
            } else if !just_sep {
                out.push('_');
                just_sep = true;
            }
        }
        if out.ends_with('_') {
            out.pop();
        }
        out
    }
    let server = clean(server);
    let tool = clean(tool);
    // A half made entirely of symbols/non-ascii (e.g. a CJK server name) would
    // clean to empty; substitute a stable placeholder so the name stays
    // well-formed. Two such tools then collide (caught by dedup_check) instead
    // of silently shadowing.
    let server = if server.is_empty() {
        "server".to_string()
    } else {
        server
    };
    let tool = if tool.is_empty() {
        "tool".to_string()
    } else {
        tool
    };
    let mut name = format!("mcp_{server}_{tool}");
    // Cap at 64 chars (Anthropic/OpenAI tool-name limit). The name is pure ASCII
    // (alnum/_), so byte truncation stays on a char boundary; truncate() is a
    // no-op when the name is already short enough.
    name.truncate(64);
    name
}

/// Reject duplicate agent-facing tool names. Sanitization can map distinct
/// (server, tool) pairs onto the same identifier; report that as a startup
/// error instead of letting one tool silently shadow another.
fn dedup_check(names: &[String]) -> Result<()> {
    let mut seen: HashSet<&str> = HashSet::new();
    for n in names {
        if !seen.insert(n.as_str()) {
            bail!(
                "MCP tool name collision after sanitization: '{n}' — two tools \
                 map to the same identifier; use more distinct server/tool names"
            );
        }
    }
    Ok(())
}

/// Hot-add collision gate: the new server's display names must not collide
/// with each other OR with any name the agent already exposes. Pure, so the
/// collision logic is testable without a live server. (The charset/length
/// halves of `validate_tool_names` are structurally guaranteed here —
/// `sanitize_name` emits only `[A-Za-z0-9_]` capped at 64.)
fn check_new_names(new: &[String], existing: &[String]) -> Result<()> {
    let mut seen: HashSet<&str> = existing.iter().map(|s| s.as_str()).collect();
    for n in new {
        if !seen.insert(n.as_str()) {
            bail!(
                "MCP tool name collision after sanitization: '{n}' — the new \
                 server's tool maps to an identifier the agent already exposes; \
                 use more distinct server/tool names"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{build, check_new_names, dedup_check, sanitize_name, McpServerConfig};
    use crate::tools::{Tool as _, ToolCtx};
    use serde_json::json;

    #[test]
    fn sanitize_keeps_clean_identifiers_unchanged() {
        assert_eq!(sanitize_name("fs", "read_file"), "mcp_fs_read_file");
        assert_eq!(sanitize_name("server_1", "tool_2"), "mcp_server_1_tool_2");
    }

    #[test]
    fn sanitize_replaces_non_alnum_runs_with_single_underscore() {
        // spaces, dots, slashes, symbols all collapse to one '_' between runs.
        assert_eq!(
            sanitize_name("my server", "do.thing"),
            "mcp_my_server_do_thing"
        );
        assert_eq!(sanitize_name("a...b", "c/d"), "mcp_a_b_c_d");
        assert_eq!(sanitize_name("---lead", "trail---"), "mcp_lead_trail");
    }

    #[test]
    fn sanitize_falls_back_when_a_half_is_all_symbols() {
        // Non-ascii-alnum halves (CJK, pure symbols) → placeholder so the name
        // stays well-formed; dedup_check then catches the resulting collision.
        assert_eq!(sanitize_name("中文", "工具"), "mcp_server_tool");
        assert_eq!(sanitize_name("***", "ok"), "mcp_server_ok");
        assert_eq!(sanitize_name("ok", "***"), "mcp_ok_tool");
    }

    #[test]
    fn sanitize_caps_long_names_at_64_chars() {
        // Anthropic tool names are {1,64}; OpenAI function names too. A long
        // server+tool combo must be truncated so the provider doesn't 400.
        // The name is ASCII, so truncation keeps the mcp_ prefix intact.
        let s = "s".repeat(40);
        let t = "t".repeat(60);
        let name = sanitize_name(&s, &t);
        assert_eq!(name.len(), 64);
        assert!(name.starts_with("mcp_"));
        // Short names are untouched.
        assert_eq!(sanitize_name("a", "b"), "mcp_a_b");
    }

    #[test]
    fn dedup_check_passes_on_unique_names() {
        assert!(dedup_check(&["mcp_a_b".into(), "mcp_c_d".into()]).is_ok());
    }

    #[test]
    fn dedup_check_rejects_collision() {
        let r = dedup_check(&["mcp_a_b".into(), "mcp_a_b".into()]);
        assert!(r.is_err());
        assert!(format!("{}", r.unwrap_err()).contains("collision"));
    }

    #[test]
    fn check_new_names_passes_when_disjoint() {
        let existing = vec!["mcp_fs_read".to_string(), "shell".to_string()];
        let new = vec!["mcp_db_query".to_string(), "mcp_db_exec".to_string()];
        assert!(check_new_names(&new, &existing).is_ok());
    }

    #[test]
    fn check_new_names_rejects_collision_with_existing() {
        let existing = vec!["mcp_a_b_c".to_string()];
        // server "a" + tool "b.c" and server "a.b" + tool "c" both sanitize
        // to mcp_a_b_c — the hot-add side must refuse, not shadow.
        let new = vec!["mcp_a_b_c".to_string()];
        let r = check_new_names(&new, &existing);
        assert!(r.is_err());
        assert!(format!("{}", r.unwrap_err()).contains("already exposes"));
    }

    #[test]
    fn check_new_names_rejects_collision_within_batch() {
        // Two tools of the SAME new server colliding (e.g. "b.c" and "b_c")
        // must also fail — `existing` alone wouldn't catch it.
        let new = vec!["mcp_a_b".to_string(), "mcp_a_b".to_string()];
        let r = check_new_names(&new, &[]);
        assert!(r.is_err());
    }

    /// 真实 MCP 端到端：起一个 keyless 的 filesystem server，枚举它的工具，
    /// 并实际执行一次 search_files。覆盖「连接 → 握手 → 枚举 → 执行」全链路，
    /// 即 TinyWorker 建 agent 时走的同一条 mcp::build 路径。
    /// `#[ignore]`：需要 npx + 网络，不进常规 CI；`cargo test filesystem_mcp -- --ignored --nocapture`。
    #[test]
    #[ignore]
    fn filesystem_mcp_connects_enumerates_and_searches() {
        let dir = std::env::temp_dir().join("ta-mcp-e2e");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("alpha.txt"), "a").unwrap();
        std::fs::write(dir.join("beta.md"), "b").unwrap();
        let server = McpServerConfig {
            name: "fs".into(),
            command: Some("npx".into()),
            args: vec![
                "-y".into(),
                "@modelcontextprotocol/server-filesystem".into(),
                dir.display().to_string(),
            ],
            url: None,
            transport: None,
            headers: Default::default(),
            env: Default::default(),
        };
        let tools = build(&[server]).expect("MCP server 应连接并枚举出工具");
        let names: Vec<String> = tools.iter().map(|t| t.def().name.clone()).collect();
        eprintln!("fs MCP 暴露的工具：{names:?}");
        assert!(
            names
                .iter()
                .any(|n| n.contains("search") || n.contains("list_directory")),
            "应含 search/list 工具，得到 {names:?}"
        );
        // 实际执行一次工具调用。list_directory 语义确定（直接列目录内容），
        // 用它断言「执行」链路通；search_files 也调一次看输出（其 pattern 匹配
        // 语义随 server 版本而异，不强断言）。
        let ctx = ToolCtx {
            workdir: dir.clone(),
            max_output_chars: 4000,
            restrict_to_workdir: false,
            cancel: None,
            shell_timeout_secs: 120,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
        };
        let list = tools
            .iter()
            .find(|t| t.def().name.contains("list_directory"))
            .expect("有 list 工具");
        let out = list
            .run(&json!({ "path": dir.display().to_string() }), &ctx)
            .expect("list_directory 应执行成功");
        eprintln!("list_directory 结果：{}", out.content);
        assert!(
            out.content.contains("alpha.txt"),
            "应列出 alpha.txt，得到：{}",
            out.content
        );
        assert!(
            out.content.contains("beta.md"),
            "应列出 beta.md，得到：{}",
            out.content
        );
        if let Some(search) = tools.iter().find(|t| t.def().name.contains("search")) {
            let s = search
                .run(
                    &json!({ "path": dir.display().to_string(), "pattern": "alpha" }),
                    &ctx,
                )
                .expect("search_files 应执行成功");
            eprintln!("search_files(alpha) 结果：{}", s.content);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
