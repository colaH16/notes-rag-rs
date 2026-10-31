# syntax=docker/dockerfile:1
#
# Runtime image for the Rust web + MCP roles ONLY. The original Python image
# also contains the indexer, OCR, monitor, snapshot and notification jobs;
# those keep running from the Python image and are NOT replaced by this one.
#
# CI builds the inputs natively on Ubuntu 24.04 runners of the same
# architecture before `docker build`:
#   target/release/notes-rag-rs   (cargo build --locked --release)
#   build/qwen-tokenizer.json     (scripts/fetch_tokenizer.sh, SHA-256 pinned)
# The binary links only glibc/libgcc (TLS is rustls + ring), matching the base.
#
# All configuration is environment variables; the entrypoint takes no
# arguments (RAG_ROLE=web|mcp). Frontend assets are embedded in the binary.
# Runs as UID/GID 4261 like the Python image, so existing PVC ownership
# (web SQLite state) keeps working. Writable paths: only SEARCH_STATE_DB's
# directory (web) and RAG_ALERT_DIR if it is configured (MCP); the root
# filesystem can be read-only.
FROM docker.io/library/ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --chmod=0755 target/release/notes-rag-rs /usr/local/bin/notes-rag-rs
COPY --chmod=0644 build/qwen-tokenizer.json /app/qwen-tokenizer.json

ENV RAG_TOKENIZER_PATH=/app/qwen-tokenizer.json
USER 4261:4261
EXPOSE 8080 8000
ENTRYPOINT ["/usr/local/bin/notes-rag-rs"]
