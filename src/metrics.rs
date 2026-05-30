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

//! Lightweight in-process metrics exposed at `/metrics` in Prometheus text
//! exposition format. Mostly monotonic `AtomicU64` counters, plus a latency
//! histogram and a live cache-occupancy gauge. All reads/writes use `Relaxed`
//! ordering — exact cross-metric consistency is not required for scrape-style
//! metrics, and the histogram's per-bucket counters are summed at render time.

use std::fmt::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// Master switch, set once at startup from `STATIC_METRICS`. There is no runtime
// toggle (this is a static file server), so it is a write-once `OnceLock<bool>`
// rather than an `AtomicBool` — the value never mutates after startup. When
// false every `record_*` is a no-op and callers skip their setup work (the
// latency clock, the cache-gauge lookup) so disabling metrics is genuinely
// zero-overhead, not just "the /metrics route is gone". Reads before
// `set_enabled` runs (there are none in practice) default to enabled.
static ENABLED: OnceLock<bool> = OnceLock::new();

static REQUESTS_TOTAL: AtomicU64 = AtomicU64::new(0);
static RESPONSES_2XX: AtomicU64 = AtomicU64::new(0);
static RESPONSES_3XX: AtomicU64 = AtomicU64::new(0);
static RESPONSES_4XX: AtomicU64 = AtomicU64::new(0);
static RESPONSES_5XX: AtomicU64 = AtomicU64::new(0);
static RESPONSE_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);

// Live in-memory cache occupancy. `CACHE_ENTRIES` is a gauge maintained by
// `record_cache_insert` (+1 per new key, -N per eviction); `CACHE_CAPACITY` is
// the configured weight limit (max entries), set once at startup so a scrape
// can compute the fill ratio.
static CACHE_ENTRIES: AtomicU64 = AtomicU64::new(0);
static CACHE_CAPACITY: AtomicU64 = AtomicU64::new(0);

// Request-latency histogram. Bucket upper bounds are in seconds and tuned for
// static serving (sub-millisecond to a few seconds): the low buckets catch the
// common cache-hit/small-file path, the tail catches large or slow responses.
// Each observation bumps exactly one slot (the buckets below are stored
// non-cumulatively); `render` emits the cumulative `le` counts Prometheus wants.
const LATENCY_BUCKETS_SECONDS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];
// One slot per finite bucket, plus a final overflow slot for the implicit
// `le="+Inf"` bucket.
const LATENCY_SLOTS: usize = LATENCY_BUCKETS_SECONDS.len() + 1;
static LATENCY_BUCKET_COUNTS: [AtomicU64; LATENCY_SLOTS] =
    [const { AtomicU64::new(0) }; LATENCY_SLOTS];
// Sum of all observed latencies in microseconds (integer-friendly; rendered as
// fractional seconds for the histogram `_sum`).
static LATENCY_SUM_MICROS: AtomicU64 = AtomicU64::new(0);

/// Record a finished request: bumps the total, the status-class bucket, the
/// logical (uncompressed) byte counter, and the latency histogram. `elapsed` is
/// the time to produce the response (handler latency), measured by the outermost
/// middleware — it does not include streaming the body to a slow client, which
/// would otherwise drown the signal in client bandwidth.
pub fn record_request(status: u16, bytes: u64, elapsed: Duration) {
    if !enabled() {
        return;
    }
    REQUESTS_TOTAL.fetch_add(1, Ordering::Relaxed);
    RESPONSE_BYTES_TOTAL.fetch_add(bytes, Ordering::Relaxed);
    observe_latency(elapsed);
    let bucket = match status / 100 {
        2 => &RESPONSES_2XX,
        3 => &RESPONSES_3XX,
        4 => &RESPONSES_4XX,
        5 => &RESPONSES_5XX,
        _ => return,
    };
    bucket.fetch_add(1, Ordering::Relaxed);
}

fn observe_latency(elapsed: Duration) {
    LATENCY_SUM_MICROS.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
    let secs = elapsed.as_secs_f64();
    // First bucket whose upper bound covers this observation; falls through to
    // the final overflow (`+Inf`) slot when it exceeds every finite bound.
    let slot = LATENCY_BUCKETS_SECONDS
        .iter()
        .position(|&bound| secs <= bound)
        .unwrap_or(LATENCY_BUCKETS_SECONDS.len());
    LATENCY_BUCKET_COUNTS[slot].fetch_add(1, Ordering::Relaxed);
}

pub fn record_cache_hit() {
    if !enabled() {
        return;
    }
    CACHE_HITS.fetch_add(1, Ordering::Relaxed);
}

pub fn record_cache_miss() {
    if !enabled() {
        return;
    }
    CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
}

/// Adjust the live cache-occupancy gauge after an insert. `new` is whether the
/// key was absent before the insert (so it adds an entry), and `evicted` is how
/// many existing entries the insert pushed out. Net change = `new as u64 -
/// evicted`. The subtraction can't underflow: you can only evict entries that
/// were already counted.
pub fn record_cache_insert(new: bool, evicted: usize) {
    if !enabled() {
        return;
    }
    if new {
        CACHE_ENTRIES.fetch_add(1, Ordering::Relaxed);
    }
    if evicted > 0 {
        CACHE_ENTRIES.fetch_sub(evicted as u64, Ordering::Relaxed);
    }
}

/// Record the configured cache capacity (max entries) once at startup.
pub fn set_cache_capacity(capacity: usize) {
    CACHE_CAPACITY.store(capacity as u64, Ordering::Relaxed);
}

/// Enable or disable all metric collection (from `STATIC_METRICS`). Called once
/// at startup. When disabled, every `record_*` returns immediately and callers
/// skip their own setup work — see `enabled`.
pub fn set_enabled(value: bool) {
    let _ = ENABLED.set(value);
}

/// Whether metric collection is on. Callers use this to skip work they only do
/// to feed metrics (timing a request, the cache-occupancy lookup) so a disabled
/// metrics subsystem costs nothing on the hot path. Defaults to enabled until
/// `set_enabled` runs at startup.
pub fn enabled() -> bool {
    ENABLED.get().copied().unwrap_or(true)
}

/// Render the current counters in Prometheus text exposition format.
pub fn render() -> String {
    let requests = REQUESTS_TOTAL.load(Ordering::Relaxed);
    let r2 = RESPONSES_2XX.load(Ordering::Relaxed);
    let r3 = RESPONSES_3XX.load(Ordering::Relaxed);
    let r4 = RESPONSES_4XX.load(Ordering::Relaxed);
    let r5 = RESPONSES_5XX.load(Ordering::Relaxed);
    let bytes = RESPONSE_BYTES_TOTAL.load(Ordering::Relaxed);
    let hits = CACHE_HITS.load(Ordering::Relaxed);
    let misses = CACHE_MISSES.load(Ordering::Relaxed);
    let cache_entries = CACHE_ENTRIES.load(Ordering::Relaxed);
    let cache_capacity = CACHE_CAPACITY.load(Ordering::Relaxed);

    let mut s = String::with_capacity(1024);
    let _ = write!(
        s,
        "# HELP static_serve_requests_total Total number of HTTP requests served.\n\
         # TYPE static_serve_requests_total counter\n\
         static_serve_requests_total {requests}\n\
         # HELP static_serve_responses_total HTTP responses by status class.\n\
         # TYPE static_serve_responses_total counter\n\
         static_serve_responses_total{{class=\"2xx\"}} {r2}\n\
         static_serve_responses_total{{class=\"3xx\"}} {r3}\n\
         static_serve_responses_total{{class=\"4xx\"}} {r4}\n\
         static_serve_responses_total{{class=\"5xx\"}} {r5}\n\
         # HELP static_serve_response_bytes_total Total logical (uncompressed) response body bytes.\n\
         # TYPE static_serve_response_bytes_total counter\n\
         static_serve_response_bytes_total {bytes}\n\
         # HELP static_serve_cache_hits_total In-memory file cache hits.\n\
         # TYPE static_serve_cache_hits_total counter\n\
         static_serve_cache_hits_total {hits}\n\
         # HELP static_serve_cache_misses_total In-memory file cache misses.\n\
         # TYPE static_serve_cache_misses_total counter\n\
         static_serve_cache_misses_total {misses}\n\
         # HELP static_serve_cache_entries Current number of entries in the in-memory file cache.\n\
         # TYPE static_serve_cache_entries gauge\n\
         static_serve_cache_entries {cache_entries}\n\
         # HELP static_serve_cache_capacity Configured in-memory file cache capacity (max entries).\n\
         # TYPE static_serve_cache_capacity gauge\n\
         static_serve_cache_capacity {cache_capacity}\n"
    );

    // Request-latency histogram: cumulative `le` buckets, then `_sum` / `_count`.
    let _ = s.write_str(
        "# HELP static_serve_request_duration_seconds Request handling latency in seconds (time to produce the response).\n\
         # TYPE static_serve_request_duration_seconds histogram\n",
    );
    let mut cumulative = 0u64;
    for (i, bound) in LATENCY_BUCKETS_SECONDS.iter().enumerate() {
        cumulative += LATENCY_BUCKET_COUNTS[i].load(Ordering::Relaxed);
        let _ = writeln!(
            s,
            "static_serve_request_duration_seconds_bucket{{le=\"{bound}\"}} {cumulative}"
        );
    }
    cumulative += LATENCY_BUCKET_COUNTS[LATENCY_BUCKETS_SECONDS.len()].load(Ordering::Relaxed);
    let sum_seconds = LATENCY_SUM_MICROS.load(Ordering::Relaxed) as f64 / 1_000_000.0;
    let _ = writeln!(
        s,
        "static_serve_request_duration_seconds_bucket{{le=\"+Inf\"}} {cumulative}\n\
         static_serve_request_duration_seconds_sum {sum_seconds}\n\
         static_serve_request_duration_seconds_count {cumulative}"
    );
    s
}
