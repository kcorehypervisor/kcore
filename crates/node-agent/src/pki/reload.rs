//! In-process TLS reload for the node-agent listener.
//!
//! `tonic` 0.12 bakes the `rustls` `ServerConfig` into the `Server` when
//! `tls_config` is called and exposes no way to swap the certificate on a live
//! server. Reload is therefore done by rebuilding the listener: the serve loop
//! in `main.rs` runs `serve_with_shutdown` with a future that completes on a
//! reload request, then loops round and reads the (new) TLS material from disk
//! again.
//!
//! This is a *process-preserving* reload: no exec, no systemd restart, no lost
//! in-memory state (live-migration bookkeeping, VMM sockets, storage handles).
//! It does close established gRPC connections, which callers already retry —
//! `serve_with_shutdown` drains in-flight requests before returning.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

/// Reason a serve iteration ended, so `main.rs` knows whether to loop or exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeExit {
    Shutdown,
    Reload,
}

/// Cloneable handle used by the rotation path to ask the listener to rebuild.
#[derive(Clone, Default)]
pub struct ReloadHandle {
    notify: Arc<Notify>,
    requests: Arc<AtomicU64>,
}

impl ReloadHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the listener to rebuild from the material now on disk. Returns the
    /// number of reloads requested so far, which tests use to assert that a
    /// rotation actually asked for one.
    ///
    /// `notify_waiters` (rather than `notify_one`) is deliberate: a reload is
    /// only meaningful for a listener that is currently serving. If none is
    /// waiting, the next `serve_with_shutdown` will read the new files anyway,
    /// so there is nothing to remember.
    pub fn request(&self) -> u64 {
        let count = self.requests.fetch_add(1, Ordering::SeqCst) + 1;
        self.notify.notify_waiters();
        count
    }

    /// Total reloads requested since start.
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::SeqCst)
    }

    /// Completes on the next [`Self::request`].
    pub async fn wait(&self) {
        self.notify.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_counts_up() {
        let handle = ReloadHandle::new();
        assert_eq!(handle.requests(), 0);
        assert_eq!(handle.request(), 1);
        assert_eq!(handle.request(), 2);
        assert_eq!(handle.requests(), 2);
    }

    #[test]
    fn clones_share_one_counter() {
        let handle = ReloadHandle::new();
        let clone = handle.clone();
        clone.request();
        assert_eq!(handle.requests(), 1);
    }

    #[tokio::test]
    async fn wait_wakes_on_request() {
        let handle = ReloadHandle::new();
        let waiter = handle.clone();
        let task = tokio::spawn(async move { waiter.wait().await });
        // Give the task a chance to register before notifying; notify_waiters
        // intentionally does not queue permits for absent waiters.
        tokio::task::yield_now().await;
        for _ in 0..50 {
            handle.request();
            if task.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        task.await.expect("waiter should wake");
    }

    #[tokio::test]
    async fn wait_without_a_request_does_not_complete() {
        let handle = ReloadHandle::new();
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(50), handle.wait()).await;
        assert!(result.is_err(), "no reload requested, must keep waiting");
    }
}
