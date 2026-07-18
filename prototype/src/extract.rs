use std::path::Path;

use anyhow::{anyhow, Context, Result};

const PDF_MIN_CHARS: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractStatus {
    Extracted,
    NeedsOcr,
    Excluded,
    Failed,
}

impl ExtractStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Extracted => "extracted",
            Self::NeedsOcr => "needs-ocr",
            Self::Excluded => "excluded",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExtractResult {
    pub status: ExtractStatus,
    pub text: Option<String>,
    pub error: Option<String>,
}

pub fn is_supported_extension(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some(
            "txt"
                | "md"
                | "markdown"
                | "rs"
                | "py"
                | "ts"
                | "tsx"
                | "js"
                | "jsx"
                | "go"
                | "java"
                | "c"
                | "h"
                | "cpp"
                | "hpp"
                | "html"
                | "htm"
                | "css"
                | "json"
                | "yaml"
                | "yml"
                | "toml"
                | "csv"
                | "pdf",
        )
    )
}

pub fn extract_file(path: &Path, max_file_bytes: u64) -> Result<ExtractResult> {
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.len() > max_file_bytes {
        return Ok(ExtractResult {
            status: ExtractStatus::Excluded,
            text: None,
            error: Some(format!(
                "file larger than max_file_bytes ({})",
                max_file_bytes
            )),
        });
    }
    if !is_supported_extension(path) {
        return Ok(ExtractResult {
            status: ExtractStatus::Excluded,
            text: None,
            error: Some("unsupported extension".into()),
        });
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let result = match ext.as_str() {
        "pdf" => extract_pdf(path),
        "html" | "htm" => extract_html(path),
        _ => extract_plain(path),
    };

    match result {
        Ok(text) => {
            let trimmed = text.trim().to_string();
            if ext == "pdf" && trimmed.chars().count() < PDF_MIN_CHARS {
                Ok(ExtractResult {
                    status: ExtractStatus::NeedsOcr,
                    text: Some(trimmed),
                    error: Some("PDF text layer too small; needs OCR".into()),
                })
            } else if trimmed.is_empty() {
                Ok(ExtractResult {
                    status: ExtractStatus::Failed,
                    text: None,
                    error: Some("extracted text empty".into()),
                })
            } else {
                Ok(ExtractResult {
                    status: ExtractStatus::Extracted,
                    text: Some(trimmed),
                    error: None,
                })
            }
        }
        // Scanned / broken PDFs often fail the text-layer parser; treat as OCR queue.
        Err(err) if ext == "pdf" => Ok(ExtractResult {
            status: ExtractStatus::NeedsOcr,
            text: None,
            error: Some(format!("PDF text extract failed; needs OCR: {err}")),
        }),
        Err(err) => Ok(ExtractResult {
            status: ExtractStatus::Failed,
            text: None,
            error: Some(err.to_string()),
        }),
    }
}

fn extract_plain(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    String::from_utf8(bytes).map_err(|e| anyhow!("utf-8 decode {}: {e}", path.display()))
}

fn extract_html(path: &Path) -> Result<String> {
    let html = extract_plain(path)?;
    Ok(html2text::from_read(html.as_bytes(), 100))
}

fn extract_pdf(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read pdf {}", path.display()))?;
    let text = pdf_extract::extract_text_from_mem(&bytes)
        .with_context(|| format!("pdf extract {}", path.display()))?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn plain_text_extracts() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("note.md");
        std::fs::write(&path, "# Hello\n\nworld").unwrap();
        let r = extract_file(&path, 8 * 1024 * 1024).unwrap();
        assert_eq!(r.status, ExtractStatus::Extracted);
        assert!(r.text.unwrap().contains("Hello"));
    }

    #[test]
    fn html_strips_tags() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("page.html");
        std::fs::write(
            &path,
            "<html><body><h1>Title</h1><p>Body text</p></body></html>",
        )
        .unwrap();
        let r = extract_file(&path, 8 * 1024 * 1024).unwrap();
        assert_eq!(r.status, ExtractStatus::Extracted);
        let text = r.text.unwrap();
        assert!(text.contains("Title"));
        assert!(text.contains("Body"));
        assert!(!text.contains("<h1>"));
    }

    #[test]
    fn oversized_excluded() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("big.txt");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&[b'a'; 100]).unwrap();
        let r = extract_file(&path, 50).unwrap();
        assert_eq!(r.status, ExtractStatus::Excluded);
    }

    #[test]
    fn unsupported_excluded() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("pic.png");
        std::fs::write(&path, b"\x89PNG").unwrap();
        let r = extract_file(&path, 8 * 1024 * 1024).unwrap();
        assert_eq!(r.status, ExtractStatus::Excluded);
    }

    #[test]
    fn empty_pdf_needs_ocr() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("blank.pdf");
        // Minimal PDF with empty content stream (may fail parse or yield no text).
        let pdf = b"%PDF-1.1\n1 0 obj<< /Type /Catalog /Pages 2 0 R >>endobj\n\
2 0 obj<< /Type /Pages /Kids [3 0 R] /Count 1 >>endobj\n\
3 0 obj<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 144] /Contents 4 0 R >>endobj\n\
4 0 obj<< /Length 0 >>stream\nendstream\nendobj\nxref\n0 5\n\
0000000000 65535 f \n0000000009 00000 n \n0000000058 00000 n \n\
0000000115 00000 n \n0000000214 00000 n \ntrailer<< /Size 5 /Root 1 0 R >>\n\
startxref\n263\n%%EOF\n";
        std::fs::write(&path, pdf).unwrap();
        let r = extract_file(&path, 8 * 1024 * 1024).unwrap();
        assert_eq!(r.status, ExtractStatus::NeedsOcr);
    }
}
