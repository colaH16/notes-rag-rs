//! HTTP/1.1 serving shared by both roles (Python: uvicorn).
//!
//! * HTTP/1.1 only, like uvicorn's h11 backend.
//! * 5 s header-read timeout, which also bounds idle keep-alive connections
//!   (`timeout_keep_alive=5`).
//! * Optional request concurrency cap answered immediately with
//!   `503 Service Unavailable` (`limit_concurrency=32` for the web role).
//!   Unlike uvicorn, idle keep-alive connections do not count toward it.
//! * SIGTERM/SIGINT: stop accepting, cancel MCP sessions, drain in-flight
//!   requests for at most the configured grace period.

use axum::Router;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::{HeaderValue, StatusCode, header};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

async fn limit_concurrency(semaphore: Arc<Semaphore>, request: Request, next: Next) -> Response {
    match semaphore.try_acquire_owned() {
        Ok(_permit) => next.run(request).await,
        Err(_) => {
            let mut response =
                (StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable").into_response();
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
            response
        }
    }
}

/// Resolves on SIGTERM or SIGINT.
async fn termination() {
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = async {
            match terminate.as_mut() {
                Some(signal) => { signal.recv().await; }
                None => std::future::pending::<()>().await,
            }
        } => {}
    }
}

pub async fn serve(
    addr: SocketAddr,
    app: Router,
    concurrency: Option<usize>,
    grace: Duration,
    shutdown: CancellationToken,
) -> Result<(), String> {
    let app = match concurrency {
        Some(limit) => {
            let semaphore = Arc::new(Semaphore::new(limit));
            app.layer(axum::middleware::from_fn(
                move |request: Request, next: Next| {
                    limit_concurrency(semaphore.clone(), request, next)
                },
            ))
        }
        None => app,
    };
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|_| format!("cannot listen on {addr}"))?;
    let graceful = GracefulShutdown::new();
    let stop = termination();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let service = TowerToHyperService::new(app.clone());
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder
                    .timer(TokioTimer::new())
                    .header_read_timeout(Duration::from_secs(5))
                    .keep_alive(true)
                    .max_buf_size(64 * 1024);
                let connection = builder.serve_connection(TokioIo::new(stream), service);
                let watched = graceful.watch(connection);
                tokio::spawn(async move {
                    let _ = watched.await;
                });
            }
            _ = &mut stop => break,
            _ = shutdown.cancelled() => break,
        }
    }
    shutdown.cancel();
    drop(listener);
    tokio::select! {
        _ = graceful.shutdown() => {}
        _ = tokio::time::sleep(grace) => {}
    }
    Ok(())
}
