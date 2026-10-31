//! Single binary, two roles selected only by environment (no arguments):
//!
//! * `RAG_ROLE=web` – public search UI / graph / reader (was `public_search.py`)
//! * `RAG_ROLE=mcp` – internal MCP + `/graph` (was `mcp_server.py`)
//!
//! Like the Python services, nothing configuration-derived is logged: only a
//! fixed readiness line, or a fixed failure line plus a non-secret reason.

use notes_rag_rs::config::Env;

fn main() -> std::process::ExitCode {
    if std::env::args_os().len() > 1 {
        eprintln!("notes-rag-rs takes no arguments; select the role with RAG_ROLE=web|mcp.");
        return std::process::ExitCode::from(2);
    }
    notes_rag_rs::install_crypto_provider();
    let env = Env::process();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return std::process::ExitCode::from(2),
    };
    let outcome = runtime.block_on(async {
        match env.get("RAG_ROLE") {
            Some("web") => notes_rag_rs::web::run(&env)
                .await
                .map_err(|e| ("Web startup failed", e)),
            Some("mcp") => notes_rag_rs::mcp::run(&env).await.map_err(|e| {
                (
                    "MCP startup failed; check private configuration and database readiness.",
                    e,
                )
            }),
            _ => Err(("RAG_ROLE must be web or mcp.", String::new())),
        }
    });
    match outcome {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err((message, reason)) => {
            if reason.is_empty() {
                eprintln!("{message}");
            } else {
                eprintln!("{message} ({reason})");
            }
            std::process::ExitCode::from(2)
        }
    }
}
