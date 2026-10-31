//! Port of `embedding_config.QwenTokenizer`: the pinned public tokenizer JSON,
//! SHA-256 verified before parsing. No model weights, no runtime download.
//!
//! The Python `tokenizers` wheel and the Rust `tokenizers` crate share the same
//! Rust core (0.23.2); counts are checked against the Python oracle in
//! `tests/oracle`. Only the query length guard (≤ 2048 tokens) and the
//! embedding input guard (≤ 32760 tokens) depend on these counts.

use crate::compat::ProducerManifest;
use crate::py::sha256_hex;
use std::path::Path;

pub struct QwenTokenizer {
    inner: tokenizers::Tokenizer,
}

impl QwenTokenizer {
    /// `verified_bytes(path)` + `Tokenizer.from_str`, with truncation and
    /// padding disabled exactly like the Python wrapper.
    pub fn load(path: &Path, manifest: &ProducerManifest) -> Result<Self, String> {
        let data =
            std::fs::read(path).map_err(|_| "cannot read the pinned tokenizer".to_string())?;
        if sha256_hex(&data) != manifest.tokenizer_sha256 {
            return Err("tokenizer checksum mismatch".into());
        }
        let mut inner = tokenizers::Tokenizer::from_bytes(&data)
            .map_err(|_| "tokenizer file cannot be parsed".to_string())?;
        inner
            .with_truncation(None)
            .map_err(|_| "cannot disable truncation".to_string())?;
        inner.with_padding(None);
        Ok(Self { inner })
    }

    /// `len(tokenizer.encode(text, add_special_tokens=False).ids)`
    pub fn count(&self, text: &str) -> Result<usize, String> {
        self.inner
            .encode(text, false)
            .map(|e| e.get_ids().len())
            .map_err(|_| "tokenizer failure".to_string())
    }
}

/// Default path when `RAG_TOKENIZER_PATH` is unset (container layout).
pub const DEFAULT_TOKENIZER_PATH: &str = "/app/qwen-tokenizer.json";
