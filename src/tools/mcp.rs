use super::{Tool, ToolCtx, ToolOutput};
use crate::config::McpServerConfig;
use crate::mcp::McpClient;
use crate::provider::{Block, ToolDef};
use anyhow::Result;
use serde_json::{json, Value};
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
        for c in result["content"].as_array().cloned().unwrap_or_default() {
            match c["type"].as_str().unwrap_or("") {
                "text" => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(c["text"].as_str().unwrap_or_default());
                }
                "image" => {
                    if image.is_none() {
                        image = Some(Block::Image {
                            media_type: c["mimeType"].as_str().unwrap_or("image/png").to_string(),
                            data: c["data"].as_str().unwrap_or_default().to_string(),
                        });
                    }
                }
                _ => {}
            }
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
    for cfg in servers {
        let (client, infos) = McpClient::start(cfg)?;
        let n = infos.len();
        let client = Arc::new(Mutex::new(client));
        for info in infos {
            out.push(Box::new(McpTool {
                client: client.clone(),
                def: ToolDef {
                    name: format!("mcp_{}_{}", cfg.name, info.name),
                    description: info.description,
                    schema: if info.input_schema.is_null() {
                        json!({"type": "object", "properties": {}})
                    } else {
                        info.input_schema
                    },
                },
                wire_name: info.name,
            }));
        }
        eprintln!("mcp: server '{}' ready ({} tools)", cfg.name, n);
    }
    Ok(out)
}
