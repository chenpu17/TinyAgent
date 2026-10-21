use crate::provider::Block;
use anyhow::{bail, Context, Result};
use base64::Engine;
use std::io::Read;
use std::path::Path;

/// Refuse documents larger than this (same cap as `load_image`; base64 inflates
/// by ~4/3, so the wire payload stays ~13 MiB — well under Anthropic's 32 MB
/// whole-request limit).
const MAX_PDF_BYTES: u64 = 10 * 1024 * 1024;

/// Read a PDF file into a base64 [`Block::Document`] — the host-side loader
/// for pushing a document into the conversation (`agent.push_user(vec![
/// load_document_block(path)?, Block::Text("…".into())])`). Mirrors
/// `tools::image::load_block`: extension check + bounded read, so a file that
/// grows mid-read can't blow past the cap. No `load_document` TOOL rides with
/// this (model-driven PDF loading is a later, optional step); the helper is
/// host API only.
pub fn load_document_block(full: &Path) -> Result<Block> {
    let ext = full
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if ext != "pdf" {
        bail!(
            "unsupported document format {:?} (PDF only): {}",
            ext,
            full.display()
        );
    }
    // Bound the read directly rather than trusting stat then reading: a file
    // that grows between metadata() and read() (a concurrent writer, or a
    // special file that reports a small length) could otherwise blow past the
    // limit and exhaust memory. Same shape as image::load_block.
    let f = std::fs::File::open(full)
        .with_context(|| format!("cannot read document: {}", full.display()))?;
    let mut raw = Vec::new();
    f.take(MAX_PDF_BYTES + 1).read_to_end(&mut raw)?;
    if raw.len() > MAX_PDF_BYTES as usize {
        bail!(
            "document too large ({} bytes, limit {}): {}",
            raw.len(),
            MAX_PDF_BYTES,
            full.display()
        );
    }
    if !raw.starts_with(b"%PDF-") {
        bail!("not a PDF file (missing %PDF- header): {}", full.display());
    }
    Ok(Block::Document {
        media_type: "application/pdf".into(),
        data: base64::engine::general_purpose::STANDARD.encode(raw),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_pdf(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("ta-doc-{name}-{}.pdf", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn loads_a_pdf_into_a_document_block() {
        let p = tmp_pdf("ok", b"%PDF-1.4\n1 0 obj << /Type /Page >> endobj\n");
        let b = load_document_block(&p).unwrap();
        std::fs::remove_file(&p).ok();
        match b {
            Block::Document { media_type, data } => {
                assert_eq!(media_type, "application/pdf");
                assert_eq!(
                    data,
                    base64::engine::general_purpose::STANDARD
                        .encode(b"%PDF-1.4\n1 0 obj << /Type /Page >> endobj\n")
                );
            }
            other => panic!("expected Document, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_pdf_extensions_and_fake_pdfs() {
        // Wrong extension → immediate bail (no read).
        let p = std::env::temp_dir().join(format!("ta-doc-txt-{}.txt", std::process::id()));
        std::fs::write(&p, b"%PDF- whatever").unwrap();
        let err = load_document_block(&p).unwrap_err();
        std::fs::remove_file(&p).ok();
        assert!(err.to_string().contains("PDF only"), "{err}");

        // .pdf name but not PDF bytes → header check catches it.
        let p = tmp_pdf("fake", b"plain text, honest");
        let err = load_document_block(&p).unwrap_err();
        std::fs::remove_file(&p).ok();
        assert!(err.to_string().contains("%PDF-"), "{err}");
    }
}
