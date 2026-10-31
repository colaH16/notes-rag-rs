//! Mixed-language compatibility with the unchanged Python indexer.
//!
//! The Python indexer stores two signatures in `notes_rag.generation`:
//!
//! * `scope_signature`  = sha256(json.dumps(sorted(allowlist_ids), ensure_ascii=False))
//! * `config_signature` = sha256(json.dumps({model, dimensions, tokenizer,
//!   tokenizer_version, target/max/overlap tokens, code_signature}, sort_keys=True))
//!
//! `code_signature` hashes the *Python source bytes* of `segmenter.py`,
//! `chunker.py` and `embedding_config.py`, and `tokenizer_version` embeds the
//! Python `tokenizers` wheel version. A Rust reader must therefore never hash
//! its own sources: it carries an explicit, pinned *producer manifest*
//! (`compat/producer-manifest.json`) generated from the Python producer commit
//! by `scripts/producer_manifest.py`. The manifest's expected signature was
//! computed by that commit's own `index_plan.config_signature()`, and this
//! module recomputes it from the manifest fields at startup and in tests. A
//! DB generation written by any other producer revision fails closed.

use crate::py::{json as pyjson, sha256_hex};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub const MANIFEST_JSON: &str = include_str!("../compat/producer-manifest.json");

#[derive(Debug, Clone, Deserialize)]
pub struct ProducerManifest {
    pub schema: u32,
    pub code_signature: String,
    pub model: String,
    pub model_alias: String,
    pub dimensions: usize,
    pub max_input_tokens: usize,
    pub tokenizer_name: String,
    pub tokenizer_revision: String,
    pub tokenizer_sha256: String,
    pub python_tokenizers_version: String,
    pub query_instruction: String,
    pub target_tokens: u32,
    pub max_tokens: u32,
    pub overlap_tokens: u32,
    pub expected_config_signature: String,
    pub producer: ProducerRef,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProducerRef {
    pub commit: String,
}

impl ProducerManifest {
    /// Load and self-verify the embedded manifest. A mismatch means the
    /// manifest was edited inconsistently; refuse to start.
    pub fn embedded() -> Result<Self, String> {
        let manifest: Self = serde_json::from_str(MANIFEST_JSON)
            .map_err(|_| "producer manifest is not valid JSON".to_string())?;
        if manifest.schema != 1 {
            return Err("unsupported producer manifest schema".into());
        }
        if manifest.config_signature() != manifest.expected_config_signature {
            return Err("producer manifest signature self-check failed".into());
        }
        Ok(manifest)
    }

    /// `QwenTokenizer.version` as computed inside the Python producer image.
    pub fn tokenizer_version(&self) -> String {
        format!(
            "{}:{}:tokenizers-{}",
            self.tokenizer_revision, self.tokenizer_sha256, self.python_tokenizers_version
        )
    }

    /// `index_plan.config_signature(...)` for this producer.
    pub fn config_signature(&self) -> String {
        config_signature(
            &self.model,
            self.dimensions,
            &self.tokenizer_name,
            &self.tokenizer_version(),
            self.target_tokens,
            self.max_tokens,
            self.overlap_tokens,
            &self.code_signature,
        )
    }

    /// `embedding_config.query_text(question)`
    pub fn query_text(&self, question: &str) -> String {
        format!("Instruct: {}\nQuery:{}", self.query_instruction, question)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn config_signature(
    model: &str,
    dimensions: usize,
    tokenizer: &str,
    tokenizer_version: &str,
    target_tokens: u32,
    max_tokens: u32,
    overlap_tokens: u32,
    code_signature: &str,
) -> String {
    let settings = json!({
        "model": model,
        "dimensions": dimensions,
        "tokenizer": tokenizer,
        "tokenizer_version": tokenizer_version,
        "target_tokens": target_tokens,
        "max_tokens": max_tokens,
        "overlap_tokens": overlap_tokens,
        "code_signature": code_signature,
    });
    sha256_hex(pyjson::dumps(
        &settings,
        pyjson::DumpOptions {
            ensure_ascii: true,
            sort_keys: true,
        },
    ))
}

/// `sha256(json.dumps(sorted(allowed), ensure_ascii=False))`
pub fn scope_signature(allowed: &BTreeSet<String>) -> String {
    let list = Value::Array(allowed.iter().cloned().map(Value::String).collect());
    sha256_hex(pyjson::dumps_unicode(&list))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_reproduces_python_signature() {
        let manifest = ProducerManifest::embedded().unwrap();
        // Computed by the pinned Python commit's own index_plan.config_signature().
        assert_eq!(
            manifest.config_signature(),
            "f9ec8b90bdd6e84f7b8900750320b33b3ecc0b1e6ddf3c70c0f1f38eeba68ec8"
        );
        assert_eq!(
            manifest.producer.commit,
            "754a97398ac1d6ef313605f84eb7249db8eb556e"
        );
    }

    #[test]
    fn any_field_change_changes_signature() {
        let manifest = ProducerManifest::embedded().unwrap();
        let mut changed = manifest.clone();
        changed.python_tokenizers_version = "0.23.3".into();
        assert_ne!(changed.config_signature(), manifest.config_signature());
        let mut changed = manifest.clone();
        changed.code_signature = "0".repeat(64);
        assert_ne!(changed.config_signature(), manifest.config_signature());
    }

    #[test]
    fn scope_signature_uses_unicode_json() {
        let ids: BTreeSet<String> = ["b".to_string(), "한".to_string(), "a".to_string()].into();
        assert_eq!(scope_signature(&ids), sha256_hex("[\"a\", \"b\", \"한\"]"));
    }
}
