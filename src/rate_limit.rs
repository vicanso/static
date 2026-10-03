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
// with a `Retry-After` hint.
//
// The buckets are split across SHARDS independently locked maps (by IP hash),
// so concurrent requests from different clients rarely contend — with a single
// map, cache-hit traffic (no I/O to hide behind) serialized on one mutex. Each
// shard sweeps its own idle buckets (fully refilled, i.e. not limiting anyone)
// every SWEEP_INTERVAL, so a sweep holds one shard's lock over 1/SHARDS of the
// entries instead of stalling every request behind an O(all IPs) pass.
//
// The map is also hard-capped (MAX_BUCKETS_PER_SHARD): with forwarded headers
// trusted from anyone, a client can mint a fresh "IP" per request, and the map
// would otherwise grow by one bucket per request until the next sweep. At the
// cap the shard first sweeps early; if it is still full, the new IP is allowed
// without being tracked (fail-open) — memory stays bounded, and refusing every
// new client during a flood would turn the limiter into the outage.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
const SHARDS: usize = 64;
// 64 shards x 16k = ~1M tracked IPs (tens of MB) at most.
const MAX_BUCKETS_PER_SHARD: usize = 16 * 1024;
// A full shard re-sweeps at most this often, so a flood of new IPs against a
// full shard costs one O(shard) pass per interval, not one per request.
const FULL_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

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
    hasher: RandomState,
    shards: Box<[Mutex<State>]>,
    // MAX_BUCKETS_PER_SHARD; a field so tests can exercise the cap cheaply.
    shard_cap: usize,
}

impl RateLimiter {
    fn new(rate: u32, burst: u32) -> Self {
        let now = Instant::now();
        Self {
            rate: rate as f64,
            // A bucket must hold at least one token or no request could ever
            // pass. `burst == 0` means "unset" and is resolved to `rate` by the
            // caller, but clamp here too as a backstop.
            capacity: burst.max(1) as f64,
            hasher: RandomState::new(),
            shards: (0..SHARDS)
                .map(|_| {
                    Mutex::new(State {
                        buckets: HashMap::new(),
                        last_sweep: now,
                    })
                })
                .collect(),
            shard_cap: MAX_BUCKETS_PER_SHARD,
        }
    }

    // Drop buckets that would have refilled to full capacity by `now` — a full
    // bucket limits nobody, so removing it is safe.
    fn sweep(&self, state: &mut State, now: Instant) {
        let (rate, cap) = (self.rate, self.capacity);
        state.buckets.retain(|_, b| {
            let elapsed = now.duration_since(b.last_refill).as_secs_f64();
            (b.tokens + elapsed * rate).min(cap) < cap
        });
        state.last_sweep = now;
    }

    // Allow (Ok) or reject with the number of seconds until a token frees up.
    // `rate` is guaranteed > 0 here (a 0 rate disables the limiter entirely in
    // `init`), so the division is safe.
    fn check(&self, ip: IpAddr) -> std::result::Result<(), u64> {
        self.check_at(ip, Instant::now())
    }

    // The body of `check`, with `now` injected so the refill/burst/retry-after
    // math is testable without a real clock.
    fn check_at(&self, ip: IpAddr, now: Instant) -> std::result::Result<(), u64> {
        let rate = self.rate;
        let cap = self.capacity;
        let shard = &self.shards[self.hasher.hash_one(ip) as usize % self.shards.len()];
        let mut state = shard.lock().unwrap_or_else(|e| e.into_inner());

        // Periodic sweep keeps the shard bounded to recently-active IPs.
        if now.duration_since(state.last_sweep) >= SWEEP_INTERVAL {
            self.sweep(&mut state, now);
        }
        if state.buckets.len() >= self.shard_cap && !state.buckets.contains_key(&ip) {
            if now.duration_since(state.last_sweep) >= FULL_SWEEP_INTERVAL {
                self.sweep(&mut state, now);
            }
            if state.buckets.len() >= self.shard_cap {
                return Ok(());
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
    }

    #[test]
    fn allows_burst_then_limits() {
        let rl = RateLimiter::new(1, 3); // 1 req/s sustained, burst of 3
        let t0 = Instant::now();
        // the full burst passes at the same instant
        assert!(rl.check_at(ip(1), t0).is_ok());
        assert!(rl.check_at(ip(1), t0).is_ok());
        assert!(rl.check_at(ip(1), t0).is_ok());
        // the next is rejected, asking the client to retry in ~1s
        assert_eq!(rl.check_at(ip(1), t0), Err(1));
    }

    #[test]
    fn refills_over_time() {
        let rl = RateLimiter::new(2, 2); // 2 req/s, burst of 2
        let t0 = Instant::now();
        assert!(rl.check_at(ip(1), t0).is_ok());
        assert!(rl.check_at(ip(1), t0).is_ok());
        assert!(rl.check_at(ip(1), t0).is_err()); // bucket empty
        // one second later the bucket has refilled by 2 tokens
        let t1 = t0 + Duration::from_secs(1);
        assert!(rl.check_at(ip(1), t1).is_ok());
        assert!(rl.check_at(ip(1), t1).is_ok());
        assert!(rl.check_at(ip(1), t1).is_err());
    }

    #[test]
    fn buckets_are_per_ip() {
        let rl = RateLimiter::new(1, 1);
        let t0 = Instant::now();
        assert!(rl.check_at(ip(1), t0).is_ok());
        assert!(rl.check_at(ip(1), t0).is_err()); // ip 1 exhausted
        assert!(rl.check_at(ip(2), t0).is_ok()); // ip 2 is independent
    }

    #[test]
    fn burst_zero_clamps_to_one() {
        // `RateLimiter::new` clamps a 0 burst to a 1-token bucket as a backstop
        // (the 0 -> rate resolution lives in `init`). One request passes, the
        // next is limited.
        let rl = RateLimiter::new(5, 0);
        let t0 = Instant::now();
        assert!(rl.check_at(ip(1), t0).is_ok());
        assert!(rl.check_at(ip(1), t0).is_err());
    }

    #[test]
    fn full_shards_fail_open_and_stay_bounded() {
        let mut rl = RateLimiter::new(1, 1);
        rl.shard_cap = 1;
        let t0 = Instant::now();
        // the first IP lands in an empty shard and is tracked: its one-token
        // bucket is spent, so a repeat is limited
        let first = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        assert!(rl.check_at(first, t0).is_ok());
        assert!(rl.check_at(first, t0).is_err());
        // a flood of new IPs never errors (full shards fail open)...
        for n in 0..2000u32 {
            let ip = IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + n));
            assert!(rl.check_at(ip, t0).is_ok());
        }
        // ...and memory stays bounded to one bucket per shard
        let tracked: usize = rl
            .shards
            .iter()
            .map(|s| s.lock().unwrap_or_else(|e| e.into_inner()).buckets.len())
            .sum();
        assert!(tracked <= rl.shards.len(), "tracked {tracked} buckets");
        // the already-tracked IP is still limited
        assert!(rl.check_at(first, t0).is_err());
    }

    #[test]
    fn sweep_frees_a_full_shard() {
        let mut rl = RateLimiter::new(10, 1);
        rl.shard_cap = 1;
        let t0 = Instant::now();
        let a = ip(1);
        assert!(rl.check_at(a, t0).is_ok());
        // well past the refill time and the sweep interval, `a`'s bucket is
        // full again and gets swept, so its shard has room for a new tracked IP
        let later = t0 + SWEEP_INTERVAL + Duration::from_secs(1);
        assert!(rl.check_at(a, later).is_ok());
        assert!(rl.check_at(a, later).is_err());
    }
}
