use crate::provider::Message;
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
    Ok(Some((s.model, s.messages)))
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
}
