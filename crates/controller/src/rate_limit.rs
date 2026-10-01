//! Per-identity token bucket for inbound gRPC.
//!
//! The limiter sits in front of revocation. Health checks are a separate
//! tonic service and are not wrapped, so a probe cannot be locked out by a
//! noisy peer and a noisy peer cannot spend the probe's budget.
//!
//! One bucket per client certificate CN. Without a certificate the remote
//! address is the key, and peers with neither share `anonymous`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tonic::{Request, Status};

use crate::config::RateLimitConfig;

/// Drop idle buckets once the map grows past this. A cluster has tens of
/// peers; the cap exists so a rotating source address cannot grow the map
/// without bound.
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

    pub fn disabled() -> Self {
        Self::from_config(&RateLimitConfig {
            enabled: false,
            requests_per_second: 1,
            burst: 1,
        })
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

/// Refill `tokens` for `elapsed_secs` at `rps`, capped at `burst`, then spend
/// one token. The pure function is what the tests pin.
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
    fn burst_is_spent_then_refill_restores_one_token() {
        let (t1, ok1) = take(2.0, 0.0, 1.0, 2.0);
        assert!(ok1);
        assert!((t1 - 1.0).abs() < f64::EPSILON);
        let (t2, ok2) = take(t1, 0.0, 1.0, 2.0);
        assert!(ok2);
        assert!(t2.abs() < f64::EPSILON);
        let (t3, ok3) = take(t2, 0.0, 1.0, 2.0);
        assert!(!ok3);
        assert!(t3.abs() < f64::EPSILON);
        let (t4, ok4) = take(t3, 1.0, 1.0, 2.0);
        assert!(ok4);
        assert!(t4.abs() < f64::EPSILON);
    }

    #[test]
    fn refill_never_exceeds_burst() {
        let (tokens, ok) = take(0.0, 10_000.0, 100.0, 5.0);
        assert!(ok);
        assert!((tokens - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn disabled_limiter_never_rejects() {
        let limiter = RateLimiter::disabled();
        for _ in 0..50 {
            limiter.admit("cn:kctl").expect("disabled");
        }
    }

    #[test]
    fn enabled_limiter_rejects_past_burst_for_one_identity_only() {
        let limiter = RateLimiter::from_config(&RateLimitConfig {
            enabled: true,
            requests_per_second: 1,
            burst: 2,
        });
        assert!(limiter.admit("cn:a").is_ok());
        assert!(limiter.admit("cn:a").is_ok());
        let err = limiter.admit("cn:a").expect_err("burst spent");
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
        assert!(limiter.admit("cn:b").is_ok(), "buckets are per identity");
    }

    #[test]
    fn a_request_without_a_peer_certificate_uses_the_anonymous_bucket() {
        let limiter = RateLimiter::from_config(&RateLimitConfig {
            enabled: true,
            requests_per_second: 1,
            burst: 1,
        });
        let mut intercept = limiter.interceptor();
        assert!(intercept(Request::new(())).is_ok());
        let err = intercept(Request::new(())).expect_err("anonymous burst is 1");
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
    }
}
