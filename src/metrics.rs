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

//! Lightweight in-process counters exposed at `/metrics` in Prometheus text
//! exposition format. All counters are monotonic `AtomicU64`; reads/writes use
//! `Relaxed` ordering — exact cross-counter consistency is not required for
//! scrape-style metrics.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};

static REQUESTS_TOTAL: AtomicU64 = AtomicU64::new(0);
static RESPONSES_2XX: AtomicU64 = AtomicU64::new(0);
static RESPONSES_3XX: AtomicU64 = AtomicU64::new(0);
static RESPONSES_4XX: AtomicU64 = AtomicU64::new(0);
static RESPONSES_5XX: AtomicU64 = AtomicU64::new(0);
static RESPONSE_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);

/// Record a finished request: bumps the total, the status-class bucket, and
/// the logical (uncompressed) byte counter.
pub fn record_request(status: u16, bytes: u64) {
    REQUESTS_TOTAL.fetch_add(1, Ordering::Relaxed);
    RESPONSE_BYTES_TOTAL.fetch_add(bytes, Ordering::Relaxed);
    let bucket = match status / 100 {
        2 => &RESPONSES_2XX,
        3 => &RESPONSES_3XX,
        4 => &RESPONSES_4XX,
        5 => &RESPONSES_5XX,
        _ => return,
    };
    bucket.fetch_add(1, Ordering::Relaxed);
}

pub fn record_cache_hit() {
    CACHE_HITS.fetch_add(1, Ordering::Relaxed);
}

pub fn record_cache_miss() {
    CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
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
         static_serve_cache_misses_total {misses}\n"
    );
    s
}
