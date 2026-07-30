use crate::provider::{Block, Message, Role};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

const SESSION_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Session {
    version: u32,
    model: String,
    messages: Vec<Message>,
}

/// Load a session file. Returns Ok(None) if the file does not exist.
pub fn load(path: &Path) -> Result<Option<(String, Vec<Message>)>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read session: {}", path.display())),
    };
    let s: Session = match serde_json::from_str(&raw) {
        Ok(s) => s,
        Err(e) => {
            // A corrupt session (truncated write, hand edit, future format)
            // must not kill --resume: warn and start fresh instead of hard-failing.
            eprintln!(
                "[warn] session file corrupt, starting fresh: {} ({e})",
                path.display()
            );
            return Ok(None);
        }
    };
    if s.version > SESSION_VERSION {
        eprintln!(
            "[warn] session file is newer (v{}) than supported (v{}), starting fresh: {}",
            s.version,
            SESSION_VERSION,
            path.display()
        );
        return Ok(None);
    }
    let mut messages = s.messages;
    repair_dangling_tool_use(&mut messages);
    Ok(Some((s.model, messages)))
}

/// Recover from a crash that left the session ending on an assistant message
/// whose `tool_use` blocks never received matching `tool_result`s. The agent
/// persists an assistant turn (with its tool_use) BEFORE running the tools,
/// then persists the tool-result user message AFTER they finish; a kill -9 in
/// that window leaves the history unpaired, and the provider rejects the next
/// request (Anthropic/OpenAI both require N tool_use ↔ N tool_result). Append
/// one `[interrupted]` result per dangling id — mirroring the cancel path's
/// `[cancelled by user]` self-heal — so --resume recovers instead of bricking.
fn repair_dangling_tool_use(messages: &mut Vec<Message>) {
    // Only an assistant LAST message can dangle: once tools finish the agent
    // appends a tool-result user message, so a trailing user/assistant-text
    // message is already well-formed.
    let dangling: Vec<String> = match messages.last() {
        Some(m) if m.role == Role::Assistant => m
            .content
            .iter()
            .filter_map(|b| match b {
                Block::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect(),
        _ => return,
    };
    if dangling.is_empty() {
        return;
    }
    let results: Vec<Block> = dangling
        .into_iter()
        .map(|id| Block::ToolResult {
            tool_use_id: id,
            content: "[interrupted — session recovered after a crash]".into(),
            image: None,
            is_error: true,
        })
        .collect();
    messages.push(Message::user(results));
}

/// Atomically persist the conversation (write temp file, then rename).
pub fn save(path: &Path, model: &str, messages: &[Message]) -> Result<()> {
    // Serialize by reference: no need to deep-clone the whole history (with
    // its base64 images and tool inputs) on every turn just to write it out.
    #[derive(Serialize)]
    struct SessionRef<'a> {
        version: u32,
        model: &'a str,
        messages: &'a [Message],
    }
    let s = SessionRef {
        version: SESSION_VERSION,
        model,
        messages,
    };
    let bytes = serde_json::to_string_pretty(&s)?;
    // Reuse the hardened atomic write (fsync + temp cleanup) used for
    // result/report files, so the session has the same crash safety.
    crate::harness::atomic_write(path, bytes.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Block, Message, Role};

    #[test]
    fn message_serde_roundtrip() {
        let msgs = vec![
            Message::user(vec![Block::Text("hi".into())]),
            Message::assistant(vec![
                Block::Text("checking".into()),
                Block::ToolUse {
                    id: "t1".into(),
                    name: "shell".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
            ]),
            Message::user(vec![Block::ToolResult {
                tool_use_id: "t1".into(),
                content: "ok".into(),
                image: Some(Box::new(Block::Image {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                })),
                is_error: false,
            }]),
        ];
        let json = serde_json::to_string(&msgs).unwrap();
        let back: Vec<Message> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 3);
        assert_eq!(back[0].role, Role::User);
        assert!(matches!(back[1].content[1], Block::ToolUse { .. }));
        assert!(matches!(back[2].content[0], Block::ToolResult { .. }));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ta-session-{}", std::process::id()));
        let path = dir.join("s.json");
        let msgs = vec![Message::user(vec![Block::Text("hello".into())])];
        save(&path, "m1", &msgs).unwrap();
        let (model, loaded) = load(&path).unwrap().unwrap();
        assert_eq!(model, "m1");
        assert_eq!(loaded.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_session_returns_none() {
        // A truncated/garbled session must degrade to "start fresh" instead
        // of killing --resume.
        let dir = std::env::temp_dir().join(format!("ta-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.json");
        std::fs::write(&path, "not valid json {{{{").unwrap();
        assert!(load(&path).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_repairs_dangling_tool_use_after_crash() {
        // A session that crashed AFTER persisting an assistant tool_use but
        // BEFORE the tool results: load must synthesize one [interrupted]
        // tool_result per dangling id, else the next request is rejected for
        // unpaired tool_use. Mirrors the cancel path's [cancelled by user].
        let dir = std::env::temp_dir().join(format!("ta-dangle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");
        let msgs = vec![
            Message::user(vec![Block::Text("do thing".into())]),
            Message::assistant(vec![
                Block::Text("ok".into()),
                Block::ToolUse {
                    id: "t1".into(),
                    name: "shell".into(),
                    input: serde_json::json!({"command":"ls"}),
                },
                Block::ToolUse {
                    id: "t2".into(),
                    name: "shell".into(),
                    input: serde_json::json!({"command":"pwd"}),
                },
            ]),
        ];
        save(&path, "m", &msgs).unwrap();
        let (_, loaded) = load(&path).unwrap().unwrap();
        // A synthetic tool-result user message is appended (3 total).
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[2].role, Role::User);
        // N:N — both dangling ids get a result, all flagged error/interrupted.
        let results: Vec<_> = loaded[2]
            .content
            .iter()
            .filter_map(|b| match b {
                Block::ToolResult { tool_use_id, is_error, content, .. } => {
                    Some((tool_use_id.clone(), *is_error, content.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2, "one result per dangling tool_use");
        assert!(results.iter().all(|(_, e, _)| *e), "all interrupted results are errors");
        assert!(results.iter().all(|(_, _, c)| c.contains("interrupted")));
        let ids: Vec<&str> = results.iter().map(|(id, _, _)| id.as_str()).collect();
        assert!(ids.contains(&"t1") && ids.contains(&"t2"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_leaves_paired_history_untouched() {
        // A normal session ending on a tool-result user message is NOT touched
        // — repair fires only when the LAST message is an assistant with
        // tool_use (the crash window). A false positive here would inject a
        // bogus extra turn into every resumed session.
        let dir = std::env::temp_dir().join(format!("ta-paired-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");
        let msgs = vec![
            Message::user(vec![Block::Text("hi".into())]),
            Message::assistant(vec![Block::ToolUse {
                id: "t1".into(),
                name: "shell".into(),
                input: serde_json::json!({}),
            }]),
            Message::user(vec![Block::ToolResult {
                tool_use_id: "t1".into(),
                content: "ok".into(),
                image: None,
                is_error: false,
            }]),
        ];
        save(&path, "m", &msgs).unwrap();
        let (_, loaded) = load(&path).unwrap().unwrap();
        assert_eq!(
            loaded.len(),
            3,
            "paired history must not gain a synthetic result"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
