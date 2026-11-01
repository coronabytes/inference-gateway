//! The HTTP server loop. `axum::serve` gives hyper no timer, so hyper's header read timeout never
//! fires and a client can hold a connection open without ever sending a request. This loop
//! configures hyper directly, from the gateway settings in effect:
//!
//! - request headers must arrive within `header_timeout_secs` (HTTP/1), and HTTP/2 connections
//!   are pinged so dead peers are dropped;
//! - the whole request body must arrive within `body_timeout_secs`, however slowly it trickles
//!   in, and may be at most `max_body_mb`;
//! - at most `max_connections` client connections are open; further ones wait in the listen
//!   backlog.
//!
//! A new connection gets the timeouts and body limit in effect when it is accepted; the
//! connection limit follows changes at once.
//!
//! Responses are not time limited here: model streams legitimately run for minutes, and the
//! upstream side already has per-deployment timeouts.

use std::{future::Future, sync::Arc, time::Duration};

use axum::{Router, extract::DefaultBodyLimit};
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::{conn::auto::Builder, graceful::GracefulShutdown},
    service::TowerToHyperService,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
};
use tower::Layer;
use tower_http::timeout::RequestBodyDeadline;

use crate::settings::Settings;

/// How long open connections get to finish after shutdown starts.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn serve(listener: TcpListener, app: Router, mut settings: watch::Receiver<Arc<Settings>>, shutdown: impl Future<Output = ()>) {
    let mut builder = Builder::new(TokioExecutor::new());
    builder.http1().timer(TokioTimer::new()).keep_alive(true);
    builder.http2().timer(TokioTimer::new()).keep_alive_interval(Some(Duration::from_secs(30))).keep_alive_timeout(Duration::from_secs(20));
    let mut limit = settings.borrow_and_update().max_connections as usize;
    let permits = Arc::new(Semaphore::new(limit));
    let graceful = GracefulShutdown::new();
    tokio::pin!(shutdown);

    loop {
        // Wait for a free connection slot before accepting, so excess clients queue in the backlog.
        let permit = tokio::select! {
            p = permits.clone().acquire_owned() => p.expect("semaphore is never closed"),
            Ok(()) = settings.changed() => {
                limit = resize(&permits, limit, settings.borrow_and_update().max_connections as usize);
                continue;
            }
            () = &mut shutdown => break,
        };
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(a) => a,
                Err(e) => {
                    // Usually out of file descriptors; back off instead of spinning.
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            () = &mut shutdown => break,
        };
        let _ = stream.set_nodelay(true);
        let s = settings.borrow().clone();
        let header_timeout = s.header_timeout();
        let mut builder = builder.clone();
        builder.http1().header_read_timeout(header_timeout);
        let service = RequestBodyDeadline::new(DefaultBodyLimit::max(s.max_body_bytes()).layer(app.clone()), s.body_timeout());
        let watcher = graceful.watcher();
        tokio::spawn(async move {
            let _permit = permit;
            // The auto builder reads the first bytes to tell HTTP/1 from HTTP/2 before hyper's own
            // header timer starts, and that read has no timeout of its own.
            match tokio::time::timeout(header_timeout, protocol_known(&stream)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return tracing::debug!(%peer, error = %e, "connection failed before a request"),
                Err(_) => return tracing::debug!(%peer, "no request within the header timeout; closing"),
            }
            let conn = builder.serve_connection_with_upgrades(TokioIo::new(stream), TowerToHyperService::new(service));
            if let Err(e) = watcher.watch(conn.into_owned()).await {
                tracing::debug!(%peer, error = %e, "connection ended with an error");
            }
        });
    }

    drop(listener);
    if tokio::time::timeout(DRAIN_TIMEOUT, graceful.shutdown()).await.is_err() {
        tracing::warn!("some connections were still open after {}s; closing them", DRAIN_TIMEOUT.as_secs());
    }
}

/// Changes the connection limit from `from` to `to` and returns the new limit. Lowered below the
/// connections that are open, it takes effect as they close.
fn resize(permits: &Arc<Semaphore>, from: usize, to: usize) -> usize {
    if to > from {
        permits.add_permits(to - from);
    } else if to < from {
        let missing = (from - to) - permits.forget_permits(from - to);
        if missing > 0 {
            let permits = permits.clone();
            tokio::spawn(async move {
                if let Ok(p) = permits.acquire_many_owned(missing as u32).await {
                    p.forget();
                }
            });
        }
    }
    to
}

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// Waits until the first bytes show the protocol: the whole HTTP/2 preface, or anything that is
/// not a prefix of it (HTTP/1), or the client closing. Bytes are peeked, not consumed.
async fn protocol_known(stream: &TcpStream) -> std::io::Result<()> {
    let mut buf = [0u8; H2_PREFACE.len()];
    loop {
        let n = stream.peek(&mut buf).await?;
        if n == 0 || n == buf.len() || buf[..n] != H2_PREFACE[..n] {
            return Ok(());
        }
        // A partial preface: peek would return the same bytes at once, so wait a little.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_connection_limit_follows_changes() {
        let permits = Arc::new(Semaphore::new(3));
        let (a, b) = (permits.clone().acquire_owned().await.unwrap(), permits.clone().acquire_owned().await.unwrap());
        // Lowered to 1 with two connections open: none may open until both have closed.
        assert_eq!(resize(&permits, 3, 1), 1);
        assert_eq!(permits.available_permits(), 0);
        drop(a);
        tokio::task::yield_now().await;
        assert_eq!(permits.available_permits(), 0);
        drop(b);
        tokio::task::yield_now().await;
        assert_eq!(permits.available_permits(), 1);
        assert_eq!(resize(&permits, 1, 4), 4);
        assert_eq!(permits.available_permits(), 4);
    }
}
