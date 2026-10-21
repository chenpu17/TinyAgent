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
    /// Messages folded out of the live history by compaction/pruning — kept
    /// for host-side undo/audit (OpenCode's "hide, don't delete"). Optional so
    /// pre-archive files load unchanged; the version stays 1 because the file
    /// is a superset (older binaries ignore the unknown key).
    #[serde(default)]
    archive: Vec<Message>,
}

/// What a successful load returns: the model, the live history, and the
/// archive of messages folded out of it (see [`save_with_archive`]).
pub type LoadedSession = (String, Vec<Message>, Vec<Message>);

/// Load a session file. Returns Ok(None) if the file does not exist.
pub fn load(path: &Path) -> Result<Option<(String, Vec<Message>)>> {
    Ok(load_with_archive(path)?.map(|(model, messages, _)| (model, messages)))
}

/// [`load`](fn.load.html) plus the archive section — messages folded out of
/// the live history by compression/pruning, in fold order, carrying their
/// stable ids. A host implementing undo/audit reads this; plain resume
/// doesn't need it.
pub fn load_with_archive(path: &Path) -> Result<Option<LoadedSession>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("cannot read session: {}", path.display()))
        }
    };
    let s: Session = match serde_json::from_str(&raw) {
        Ok(s) => s,
        Err(e) => {
            // A corrupt session (truncated write, hand edit) must not kill
            // --resume: warn and start fresh instead of hard-failing. But
            // "fresh" with the bad file left IN PLACE meant the next save()
            // silently overwrote the user's only copy of a long history —
            // rename it aside FIRST so the scene survives for manual
            // recovery (best-effort: a failed rename degrades to the old
            // warn-only behavior, never blocks startup).
            let kept = preserve_bad_session(path);
            crate::diag::diag_warn!(
                "[warn] session file corrupt, starting fresh: {} ({e}){}",
                path.display(),
                kept.as_ref().map_or_else(String::new, |p| format!(
                    " — original preserved at {}",
                    p.display()
                ))
            );
            return Ok(None);
        }
    };
    if s.version > SESSION_VERSION {
        // Same preservation as the corrupt branch: a future-version file is
        // unreadable today but perfectly valid for the binary that wrote it —
        // leaving it in place would let the next save() clobber it before the
        // user ever downgrades back or files a report.
        let kept = preserve_bad_session(path);
        crate::diag::diag_warn!(
            "[warn] session file is newer (v{}) than supported (v{}), starting fresh: {}{}",
            s.version,
            SESSION_VERSION,
            path.display(),
            kept.as_ref().map_or_else(String::new, |p| format!(
                " — original preserved at {}",
                p.display()
            ))
        );
        return Ok(None);
    }
    let mut messages = s.messages;
    repair_dangling_tool_use(&mut messages);
    Ok(Some((s.model, messages, s.archive)))
}

/// Rename an unloadable session file aside so the next `save()` cannot
/// overwrite it: `<name>.corrupt`, then `.corrupt.1`, `.corrupt.2`, … — the
/// first free slot wins, so repeated crashes never clobber earlier evidence.
/// Returns the path the file was moved to, or `None` when preservation
/// failed: rename errors (read-only dir, AV lock on Windows, …) degrade to
/// warn-only — session AVAILABILITY outranks evidence-keeping, load still
/// proceeds fresh. The exists() probe races a concurrent writer at most into
/// the next slot (on Unix a hit rename overwrites; on Windows it errors into
/// the same fallback) — best-effort by design, this is crash forensics, not
/// transactional storage.
fn preserve_bad_session(path: &Path) -> Option<std::path::PathBuf> {
    let base = path.as_os_str().to_owned();
    for i in 0..100u32 {
        let mut name = base.clone();
        if i == 0 {
            name.push(".corrupt");
        } else {
            name.push(format!(".corrupt.{i}"));
        }
        let candidate = std::path::PathBuf::from(name);
        if candidate.exists() {
            continue;
        }
        match std::fs::rename(path, &candidate) {
            Ok(()) => return Some(candidate),
            // Lost a race into this slot (or it appeared between probe and
            // rename): try the next one.
            Err(_) if candidate.exists() => continue,
            // Genuinely cannot move the file (permissions, lock): say so and
            // give up — never block the load path.
            Err(e) => {
                crate::diag::diag_warn!(
                    "[warn] could not preserve corrupt session {} aside: {e} — it will be \
                     overwritten by the next save",
                    path.display()
                );
                return None;
            }
        }
    }
    crate::diag::diag_warn!(
        "[warn] could not preserve corrupt session {} aside: all .corrupt slots taken — it \
         will be overwritten by the next save",
        path.display()
    );
    None
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
    save_with_archive(path, model, messages, &[])
}

/// [`save`](fn.save.html) with the archive section written alongside the
/// live history. An empty archive serializes without the key — byte-identical
/// to pre-archive files.
pub fn save_with_archive(
    path: &Path,
    model: &str,
    messages: &[Message],
    archive: &[Message],
) -> Result<()> {
    // Serialize by reference: no need to deep-clone the whole history (with
    // its base64 images and tool inputs) on every turn just to write it out.
    #[derive(Serialize)]
    struct SessionRef<'a> {
        version: u32,
        model: &'a str,
        messages: &'a [Message],
        #[serde(skip_serializing_if = "<[Message]>::is_empty")]
        archive: &'a [Message],
    }
    let s = SessionRef {
        version: SESSION_VERSION,
        model,
        messages,
        archive,
    };
    let bytes = serde_json::to_string_pretty(&s)?;
    // Reuse the hardened atomic write (fsync + temp cleanup) used for
    // result/report files, so the session has the same crash safety.
    crate::fsutil::atomic_write(path, bytes.as_bytes())
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
    fn document_block_roundtrips_as_snake_case_tag() {
        // A PDF document block must survive save/load and pin its wire shape:
        // snake_case variant tag `document` with media_type/data fields (the
        // session-file format is load-bearing across versions — a rename here
        // would silently orphan existing sessions' documents).
        let msgs = vec![Message::user(vec![
            Block::Document {
                media_type: "application/pdf".into(),
                data: "JVBERi0=".into(),
            },
            Block::Text("summarize".into()),
        ])];
        let json = serde_json::to_string(&msgs).unwrap();
        assert!(
            json.contains(r#""document":{"media_type":"application/pdf","data":"JVBERi0="}"#),
            "wire shape pinned: {json}"
        );
        let back: Vec<Message> = serde_json::from_str(&json).unwrap();
        match &back[0].content[0] {
            Block::Document { media_type, data } => {
                assert_eq!(media_type, "application/pdf");
                assert_eq!(data, "JVBERi0=");
            }
            other => panic!("expected Document, got {other:?}"),
        }
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
    fn archive_roundtrip_and_legacy_compat() {
        // The archive section is format-additive: files WITH it roundtrip
        // (stable ids along), files WITHOUT it (pre-archive binaries) load
        // with an empty archive, an empty archive serializes WITHOUT the key
        // (byte-compatible with the old format), and plain `load` on an
        // archived file just drops the archive it doesn't ask for.
        let dir = std::env::temp_dir().join(format!("ta-arch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");

        // Empty archive → no key in the file (old-format shape).
        let live = vec![Message::user(vec![Block::Text("live".into())])];
        save(&path, "m", &live).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("\"archive\""),
            "empty archive omits the key: {raw}"
        );

        // Legacy file loads with an empty archive.
        let (_, _, archive) = load_with_archive(&path).unwrap().unwrap();
        assert!(archive.is_empty());

        // Archived save roundtrips, ids verbatim.
        let mut m = Message::assistant(vec![Block::Text("folded".into())]);
        m.id = Some("m7".into());
        save_with_archive(&path, "m", &live, &[m]).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"archive\""));
        let (_, l, archive) = load_with_archive(&path).unwrap().unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(archive.len(), 1);
        assert_eq!(archive[0].id.as_deref(), Some("m7"));

        // Plain load on an archived file drops the archive section.
        let (_, l) = load(&path).unwrap().unwrap();
        assert_eq!(l.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn message_ids_roundtrip_and_old_format_loads() {
        // id is serialized when present and survives save/load verbatim
        // (stable anchoring across restarts); an id-less message stays
        // byte-identical (key omitted), and a PRE-id session file (no id
        // key at all) still loads — the field defaults to None.
        let msgs = vec![Message::user(vec![Block::Text("hi".into())])];
        let json_idless = serde_json::to_string(&msgs).unwrap();
        assert!(
            !json_idless.contains("\"id\""),
            "id-less messages serialize without the key: {json_idless}"
        );
        let mut with_id = Message::assistant(vec![Block::Text("a".into())]);
        with_id.id = Some("m42".into());
        let json = serde_json::to_string(&[with_id]).unwrap();
        let back: Vec<Message> = serde_json::from_str(&json).unwrap();
        assert_eq!(back[0].id.as_deref(), Some("m42"));
        // Pre-id format: an id-less message serializes to EXACTLY what older
        // versions wrote (the key is omitted, not nulled), so those files
        // still load — the field defaults to None.
        let legacy: Vec<Message> = serde_json::from_str(&json_idless).unwrap();
        assert_eq!(legacy[0].id, None);
        assert_eq!(legacy[0].role, Role::User);
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
    fn corrupt_session_is_preserved_aside_before_fresh_start() {
        // "Start fresh" on a corrupt file must FIRST rename the bad file out
        // of the way: the old behavior left it in place for the next save()
        // to overwrite, silently destroying the only copy of a long history.
        // Slot 1 keeps the original bytes verbatim; a SECOND corrupt load
        // takes `.corrupt.1` without touching slot 1's evidence; and with
        // the original path freed, the very next save() succeeds normally.
        let dir = std::env::temp_dir().join(format!("ta-keep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");

        // First crash: garbage in, fresh-start out, evidence preserved.
        std::fs::write(&path, "garbage one {{{{").unwrap();
        assert!(load(&path).unwrap().is_none(), "corrupt → fresh start");
        assert!(!path.exists(), "original moved aside");
        let kept = dir.join("s.json.corrupt");
        assert_eq!(
            std::fs::read_to_string(&kept).unwrap(),
            "garbage one {{{{",
            "preserved copy is byte-identical to the corrupt original"
        );

        // Second crash: numbered slot, first slot's evidence untouched.
        std::fs::write(&path, "garbage two ]]]]").unwrap();
        assert!(load(&path).unwrap().is_none());
        let kept2 = dir.join("s.json.corrupt.1");
        assert_eq!(std::fs::read_to_string(&kept2).unwrap(), "garbage two ]]]]");
        assert_eq!(
            std::fs::read_to_string(&kept).unwrap(),
            "garbage one {{{{",
            "slot 1 must not be clobbered by slot 2"
        );

        // The original path is free again — the post-crash save works.
        let msgs = vec![Message::user(vec![Block::Text("recovered".into())])];
        save(&path, "m", &msgs).unwrap();
        let (_, loaded) = load(&path).unwrap().unwrap();
        assert_eq!(loaded.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn future_version_session_is_preserved_aside_before_fresh_start() {
        // A future-version file is unreadable here but valid for the binary
        // that wrote it — same preservation as garbage: rename aside (not
        // left in place for the next save to clobber), then fresh start.
        let dir = std::env::temp_dir().join(format!("ta-future-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");
        let raw = serde_json::json!({
            "version": 9999,
            "model": "m",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "from the future"}]}
            ]
        })
        .to_string();
        std::fs::write(&path, &raw).unwrap();
        assert!(
            load(&path).unwrap().is_none(),
            "future version → fresh start"
        );
        assert!(!path.exists(), "original moved aside");
        let kept = dir.join("s.json.corrupt");
        assert_eq!(
            std::fs::read_to_string(&kept).unwrap(),
            raw,
            "preserved copy keeps the future-version payload verbatim"
        );
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
                Block::ToolResult {
                    tool_use_id,
                    is_error,
                    content,
                    ..
                } => Some((tool_use_id.clone(), *is_error, content.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2, "one result per dangling tool_use");
        assert!(
            results.iter().all(|(_, e, _)| *e),
            "all interrupted results are errors"
        );
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
