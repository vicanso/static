// Copyright 2025-2026 Tree xie.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Per-IP token-bucket rate limiting for DoS resistance. Disabled by default
// (`STATIC_RATE_LIMIT=0`); when off, `check` returns before taking any lock, so
// it costs nothing on the hot path. The client IP is the one resolved by the
// caller (`ClientIp`, X-Forwarded-For / X-Real-Ip aware), so limiting matches
// the existing IP allow/block semantics rather than a second IP-extraction path.
//
// Each IP gets a bucket of `capacity` (= burst) tokens refilled at `rate`
// tokens/second; one request spends one token, and an empty bucket yields a 429
// with a `Retry-After` hint. A single mutex guards the whole map: the critical
// section is a few float ops (the periodic sweep aside), and the per-request
// file I/O dominates, so contention is negligible for the deployments that opt
// in. Idle buckets (fully refilled, i.e. not limiting anyone) are swept every
// SWEEP_INTERVAL so the map stays bounded to recently-active IPs.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

struct State {
    buckets: HashMap<IpAddr, Bucket>,
    last_sweep: Instant,
}

pub struct RateLimiter {
    rate: f64,
    capacity: f64,
    state: Mutex<State>,
}

impl RateLimiter {
    fn new(rate: u32, burst: u32) -> Self {
        Self {
            rate: rate as f64,
            // A bucket must hold at least one token or no request could ever
            // pass. `burst == 0` means "unset" and is resolved to `rate` by the
            // caller, but clamp here too as a backstop.
            capacity: burst.max(1) as f64,
            state: Mutex::new(State {
                buckets: HashMap::new(),
                last_sweep: Instant::now(),
            }),
        }
    }

    // Allow (Ok) or reject with the number of seconds until a token frees up.
    // `rate` is guaranteed > 0 here (a 0 rate disables the limiter entirely in
    // `init`), so the division is safe.
    fn check(&self, ip: IpAddr) -> std::result::Result<(), u64> {
        let now = Instant::now();
        let rate = self.rate;
        let cap = self.capacity;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());

        // Periodic sweep: drop buckets that would have refilled to full
        // capacity by now — a full bucket limits nobody, so removing it is
        // safe and keeps the map bounded to active IPs.
        if now.duration_since(state.last_sweep) >= SWEEP_INTERVAL {
            state.buckets.retain(|_, b| {
                let elapsed = now.duration_since(b.last_refill).as_secs_f64();
                (b.tokens + elapsed * rate).min(cap) < cap
            });
            state.last_sweep = now;
        }

        let bucket = state.buckets.entry(ip).or_insert_with(|| Bucket {
            tokens: cap,
            last_refill: now,
        });
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(cap);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let secs = ((1.0 - bucket.tokens) / rate).ceil() as u64;
            Err(secs.max(1))
        }
    }
}

static LIMITER: OnceLock<Option<RateLimiter>> = OnceLock::new();

// Initialize the global limiter once at startup. `rate == 0` disables it
// (stores `None`), making `check` a no-op. Calling more than once is a no-op
// after the first (only `main` calls it).
pub fn init(rate: u32, burst: u32) {
    let limiter = if rate == 0 {
        None
    } else {
        let burst = if burst == 0 { rate } else { burst };
        Some(RateLimiter::new(rate, burst))
    };
    let _ = LIMITER.set(limiter);
}

// Check a request originating from `ip`. Returns `None` to allow it, or
// `Some(retry_after_secs)` to reject it with `429 Too Many Requests`. Always
// `None` (and lock-free) when rate limiting is disabled or uninitialized.
pub fn check(ip: IpAddr) -> Option<u64> {
    match LIMITER.get() {
        Some(Some(limiter)) => limiter.check(ip).err(),
        _ => None,
    }
}
