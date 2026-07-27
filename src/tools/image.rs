use super::{resolve, Tool, ToolCtx, ToolOutput};
use crate::provider::{Block, ToolDef};
use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;

/// Refuse images larger than this (base64 inflates by ~4/3).
const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

pub struct LoadImage;

/// Detect media type from extension; returns None for unsupported formats.
pub(crate) fn media_type_for(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Read an image file into a base64 Block (shared by the tool and --image flag).
pub(crate) fn load_block(full: &Path) -> Result<Block> {
    let media_type = media_type_for(full)
        .with_context(|| format!("unsupported image format: {}", full.display()))?;
    // Bound the read directly rather than trusting stat then reading: a file
    // that grows between metadata() and read() (a concurrent writer, or a
    // special file that reports a small length) could otherwise blow past the
    // limit and exhaust memory.
    let f = std::fs::File::open(full)
        .with_context(|| format!("cannot read image: {}", full.display()))?;
    let mut raw = Vec::new();
    f.take(MAX_IMAGE_BYTES + 1).read_to_end(&mut raw)?;
    if raw.len() > MAX_IMAGE_BYTES as usize {
        bail!(
            "image too large ({} bytes, limit {}): {}",
            raw.len(),
            MAX_IMAGE_BYTES,
            full.display()
        );
    }
    Ok(Block::Image {
        media_type: media_type.to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(raw),
    })
}

impl Tool for LoadImage {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "load_image".into(),
            description: "Load a local image file (png/jpeg/gif/webp) so you can see it. \
                          Returns the image plus a short note."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Image path relative to the working directory"}
                },
                "required": ["path"]
            }),
        }
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path = input["path"].as_str().context("load_image: missing \"path\"")?;
        let full = resolve(ctx, path)?;
        let block = load_block(&full)?;
        Ok(ToolOutput {
            content: format!("[image loaded: {}]", full.display()),
            image: Some(block),
            is_error: false,
            usage: None,
        })
    }
}
