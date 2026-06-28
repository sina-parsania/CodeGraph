//! Declare-early LLM client trait abstraction (M1). No implementation lives here;
//! M5a depends on these traits (mock-tested), and M8 supplies the concrete
//! `OpenAiCompatBackend` / vision implementations behind the SAME traits.

/// A text-generating LLM (local server or cloud). Object-safe so the ingest /
/// query / enrichment code can hold `Option<&dyn LlmClient>` and degrade to the
/// no-LLM path when it is `None`.
pub trait LlmClient {
    fn generate(&self, prompt: &str, max_tokens: usize) -> Option<String>;
}

/// A vision-capable model. Used by M5a's `ImageIngestor`/`VideoIngestor` when the
/// media gate is on AND a vision model is available; otherwise the degraded
/// OCR+EXIF+filename path runs and this is never called.
pub trait VisionLlmClient {
    fn describe_image(&self, image: &[u8], prompt: &str) -> Option<String>;
}
