//! Per-identity token bucket for inbound gRPC on the node-agent.
//!
//! Same rule as the controller: one bucket per client certificate CN, the
//! remote address when there is no certificate, and `anonymous` when there
//! is neither. The health service is not wrapped.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tonic::{Request, Status};

use crate::config::RateLimitConfig;

const MAX_BUCKETS: usize = 4096;
const IDLE_SECS: u64 = 600;

#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    enabled: bool,
    rps: f64,
    burst: f64,
    buckets: HashMap<String, Bucket>,
}

struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl RateLimiter {
    pub fn from_config(cfg: &RateLimitConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                enabled: cfg.enabled,
                rps: f64::from(cfg.requests_per_second),
                burst: f64::from(cfg.burst),
                buckets: HashMap::new(),
            })),
        }
    }

    pub fn interceptor(&self) -> impl FnMut(Request<()>) -> Result<Request<()>, Status> + Clone {
        let limiter = self.clone();
        move |request: Request<()>| {
            let key = peer_key(&request);
            limiter.admit(&key)?;
            Ok(request)
        }
    }

    fn admit(&self, key: &str) -> Result<(), Status> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !inner.enabled {
            return Ok(());
        }
        let now = Instant::now();
        if inner.buckets.len() >= MAX_BUCKETS {
            inner
                .buckets
                .retain(|_, b| now.saturating_duration_since(b.updated).as_secs() < IDLE_SECS);
            if inner.buckets.len() >= MAX_BUCKETS {
                inner.buckets.clear();
            }
        }
        let burst = inner.burst;
        let rps = inner.rps;
        let bucket = inner.buckets.entry(key.to_string()).or_insert(Bucket {
            tokens: burst,
            updated: now,
        });
        let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
        let (tokens, allowed) = take(bucket.tokens, elapsed, rps, burst);
        bucket.tokens = tokens;
        bucket.updated = now;
        if allowed {
            Ok(())
        } else {
            Err(Status::resource_exhausted(
                "rate limit exceeded for this identity",
            ))
        }
    }
}

pub fn take(tokens: f64, elapsed_secs: f64, rps: f64, burst: f64) -> (f64, bool) {
    let filled = (tokens + elapsed_secs * rps).clamp(0.0, burst);
    if filled >= 1.0 {
        (filled - 1.0, true)
    } else {
        (filled, false)
    }
}

fn peer_key<T>(request: &Request<T>) -> String {
    if let Some((_, cn)) = crate::pki::revocation::peer_cert_identity(request) {
        let cn = cn.trim();
        if !cn.is_empty() {
            return format!("cn:{cn}");
        }
    }
    use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};
    if let Some(tls) = request.extensions().get::<TlsConnectInfo<TcpConnectInfo>>() {
        if let Some(addr) = tls.get_ref().remote_addr() {
            return format!("addr:{addr}");
        }
    }
    if let Some(tcp) = request.extensions().get::<TcpConnectInfo>() {
        if let Some(addr) = tcp.remote_addr() {
            return format!("addr:{addr}");
        }
    }
    "anonymous".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_reject_then_refill() {
        let (t1, ok1) = take(1.0, 0.0, 1.0, 1.0);
        assert!(ok1);
        assert!(t1.abs() < f64::EPSILON);
        let (_, ok2) = take(t1, 0.0, 1.0, 1.0);
        assert!(!ok2);
        let (_, ok3) = take(t1, 1.0, 1.0, 1.0);
        assert!(ok3);
    }
}
