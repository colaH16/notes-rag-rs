//! Rust port of the notes-rag web gateway (`RAG_ROLE=web`) and the internal
//! read-only MCP retrieval service (`RAG_ROLE=mcp`).
//!
//! Indexing, chunking, OCR, backups, notifications and jobs remain in the
//! original Python implementation; this crate only *reads* what the Python
//! indexer produced, and refuses index generations it was not built for
//! (see [`compat`]).

pub mod blog;
pub mod compat;
pub mod config;
pub mod embedding;
pub mod fault;
pub mod graph;
pub mod mcp;
pub mod native;
pub mod policy;
pub mod py;
pub mod reader;
pub mod scope;
pub mod search;
pub mod serve;
pub mod timefmt;
pub mod tokenizer;
pub mod web;

/// rustls needs one process-wide crypto provider (ring, like the TLS stack
/// used everywhere else in this binary).
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
