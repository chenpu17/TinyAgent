use super::{Tool, ToolCtx, ToolOutput};
use crate::config::McpServerConfig;
use crate::mcp::McpClient;
use crate::provider::{Block, ToolDef};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// An MCP tool exposed to the agent as `mcp_<server>_<tool>`.
struct McpTool {
    client: Arc<Mutex<McpClient>>,
    wire_name: String,
    def: ToolDef,
}

impl Tool for McpTool {
    fn def(&self) -> ToolDef {
        self.def.clone()
    }

    fn run(&self, input: &Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let result = self.client.lock().unwrap().call(
            "tools/call",
            json!({"name": self.wire_name, "arguments": input}),
        )?;

        let mut text = String::new();
        let mut image = None;
        let mut extra_images = 0u32;
        for c in result["content"].as_array().cloned().unwrap_or_default() {
            match c["type"].as_str().unwrap_or("") {
                "text" => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(c["text"].as_str().unwrap_or_default());
                }
                "image" => {
                    // ToolOutput carries a single image; keep the first and
                    // note the rest rather than silently dropping them.
                    if image.is_none() {
                        image = Some(Block::Image {
                            media_type: c["mimeType"].as_str().unwrap_or("image/png").to_string(),
                            data: c["data"].as_str().unwrap_or_default().to_string(),
                        });
                    } else {
                        extra_images += 1;
                    }
                }
                _ => {}
            }
        }
        if extra_images > 0 {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("[{extra_images} additional image(s) omitted]"));
        }
        if text.is_empty() && image.is_none() {
            text = "[empty tool result]".into();
        }
        Ok(ToolOutput {
            content: text,
            image,
            is_error: result["isError"].as_bool().unwrap_or(false),
        })
    }
}

/// Start every configured MCP server and collect their tools.
/// A server that fails the handshake aborts startup with a clear error.
pub fn build(servers: &[McpServerConfig]) -> Result<Vec<Box<dyn Tool>>> {
    let mut out: Vec<Box<dyn Tool>> = Vec::new();
    let mut display_names: Vec<String> = Vec::new();
    for cfg in servers {
        let (client, infos) = McpClient::start(cfg)?;
        let n = infos.len();
        let client = Arc::new(Mutex::new(client));
        for info in infos {
            let display = sanitize_name(&cfg.name, &info.name);
            display_names.push(display.clone());
            out.push(Box::new(McpTool {
                client: client.clone(),
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
            }));
        }
        eprintln!("mcp: server '{}' ready ({} tools)", cfg.name, n);
    }
    // Cross-server collisions after sanitization are the real risk — e.g.
    // server "a" + tool "b.c" and server "a.b" + tool "c" both yield
    // `mcp_a_b_c`. Surface it at startup rather than silently shadowing a tool.
    dedup_check(&display_names)?;
    Ok(out)
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
    let server = if server.is_empty() { "server".to_string() } else { server };
    let tool = if tool.is_empty() { "tool".to_string() } else { tool };
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

#[cfg(test)]
mod tests {
    use super::{dedup_check, sanitize_name};

    #[test]
    fn sanitize_keeps_clean_identifiers_unchanged() {
        assert_eq!(sanitize_name("fs", "read_file"), "mcp_fs_read_file");
        assert_eq!(sanitize_name("server_1", "tool_2"), "mcp_server_1_tool_2");
    }

    #[test]
    fn sanitize_replaces_non_alnum_runs_with_single_underscore() {
        // spaces, dots, slashes, symbols all collapse to one '_' between runs.
        assert_eq!(sanitize_name("my server", "do.thing"), "mcp_my_server_do_thing");
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
}
