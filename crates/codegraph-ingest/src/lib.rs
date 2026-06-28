//! Ingest layer: turn PDFs and web pages into Document chunks for the graph.
//! Text paths (PDF, web) need no system libraries. Audio/video/image ingestion
//! is feature-gated (`media`) because it needs ffmpeg/whisper/tesseract — see
//! the README roadmap; without the feature, media inputs return a clear error.

use std::path::Path;
use std::time::Duration;

pub struct DocChunk {
    pub content_type: String,
    pub source: String,
    pub text: String,
}

pub fn ingest(arg: &str) -> Result<Vec<DocChunk>, String> {
    if arg.starts_with("http://") || arg.starts_with("https://") {
        return ingest_web(arg);
    }
    let p = Path::new(arg);
    let ext = p.extension().and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase());
    match ext.as_deref() {
        Some("pdf") => ingest_pdf(p),
        Some(e) if is_text_ext(e) => {
            let text = std::fs::read_to_string(p).map_err(|e| e.to_string())?;
            Ok(chunk(&text, e, arg))
        }
        Some("png") | Some("jpg") | Some("jpeg") | Some("webp") | Some("bmp") | Some("tiff")
        | Some("tif") | Some("gif") => ingest_image_arm(p),
        Some("mp3") | Some("wav") | Some("m4a") | Some("mp4") | Some("mov") => Err(format!(
            "audio/video ingest ({}) requires a build with `--features media` (ffmpeg/whisper) - roadmap",
            arg
        )),
        _ => Err(format!(
            "unsupported ingest input: {} (pdf, text/data files: md, txt, json, jsonl, log, yaml, toml, csv, xml, html, …, or an http(s) url)",
            arg
        )),
    }
}

/// Plain-text and structured-text formats we ingest as Document chunks.
pub fn is_text_ext(ext: &str) -> bool {
    matches!(
        ext,
        "txt" | "text" | "md" | "markdown" | "mdx" | "rst" | "adoc" | "asciidoc" | "org"
            | "json" | "jsonl" | "ndjson" | "log" | "yaml" | "yml" | "toml" | "csv" | "tsv"
            | "xml" | "html" | "htm" | "ini" | "conf" | "cfg" | "properties" | "env" | "sql"
            | "tex" | "srt" | "vtt"
            // localization / i18n
            | "strings" | "stringsdict" | "po" | "pot" | "xliff" | "xlf" | "resx" | "resw"
            | "arb" | "ftl"
    )
}

/// Public entry to chunk arbitrary text into Document chunks (used by the indexer).
pub fn chunk_text(text: &str, content_type: &str, source: &str) -> Vec<DocChunk> {
    chunk(text, content_type, source)
}

#[cfg(feature = "media")]
fn ingest_image_arm(p: &Path) -> Result<Vec<DocChunk>, String> {
    ingest_image(p)
}

#[cfg(not(feature = "media"))]
fn ingest_image_arm(p: &Path) -> Result<Vec<DocChunk>, String> {
    Err(format!("image OCR requires a build with `--features media` (tesseract): {}", p.display()))
}

/// OCR an image via tesseract into a Document chunk (media feature).
#[cfg(feature = "media")]
pub fn ingest_image(path: &Path) -> Result<Vec<DocChunk>, String> {
    let p = path.to_str().ok_or("non-utf8 path")?;
    let text = tesseract::Tesseract::new(None, Some("eng"))
        .map_err(|e| e.to_string())?
        .set_image(p)
        .map_err(|e| e.to_string())?
        .get_text()
        .map_err(|e| e.to_string())?;
    Ok(chunk(&text, "image", &path.to_string_lossy()))
}

pub fn ingest_pdf(path: &Path) -> Result<Vec<DocChunk>, String> {
    let text = pdf_extract::extract_text(path).map_err(|e| e.to_string())?;
    Ok(chunk(&text, "pdf", &path.to_string_lossy()))
}

pub fn ingest_web(url: &str) -> Result<Vec<DocChunk>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("codegraph-ingest")
        .build()
        .map_err(|e| e.to_string())?;
    let html = client.get(url).send().map_err(|e| e.to_string())?.text().map_err(|e| e.to_string())?;
    let text = html2text::from_read(html.as_bytes(), 100).map_err(|e| e.to_string())?;
    Ok(chunk(&text, "web", url))
}

fn chunk(text: &str, ctype: &str, source: &str) -> Vec<DocChunk> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for para in text.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        if buf.len() + para.len() > 1500 && !buf.is_empty() {
            out.push(DocChunk { content_type: ctype.into(), source: source.into(), text: std::mem::take(&mut buf) });
        }
        buf.push_str(para);
        buf.push_str("\n\n");
    }
    if !buf.trim().is_empty() {
        out.push(DocChunk { content_type: ctype.into(), source: source.into(), text: buf.trim().to_string() });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_ext_coverage() {
        for e in ["md", "txt", "json", "jsonl", "log", "yaml", "csv", "xml", "strings", "po", "xliff"] {
            assert!(is_text_ext(e), "{e} should be ingestable text");
        }
        for e in ["png", "jpg", "pdf", "mp4", "rs"] {
            assert!(!is_text_ext(e), "{e} should not be plain text");
        }
    }

    #[test]
    fn chunk_text_preserves_content_and_type() {
        let chunks = chunk_text("alpha line\n\nbeta line", "json", "a.json");
        assert!(!chunks.is_empty());
        assert_eq!(chunks[0].content_type, "json");
        assert!(chunks.iter().any(|c| c.text.contains("alpha")));
    }
}
