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

use crate::error::{Error, Result};
use crate::metrics;
use crate::storage::get_storage;
use aho_corasick::AhoCorasick;
use async_compression::Level;
use async_compression::tokio::bufread::{BrotliEncoder, GzipEncoder, ZstdEncoder};
use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use bytesize::ByteSize;
use httpdate::{fmt_http_date, parse_http_date};
use std::collections::HashMap;
use std::fmt::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tinyufo::TinyUfo;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tokio_util::io::ReaderStream;
use tower_http::compression::CompressionLevel;

pub static X_ORIGINAL_SIZE_HEADER_NAME: HeaderName = HeaderName::from_static("x-original-size");

// Chunk size for streamed file responses (files >= STATIC_READ_MAX_SIZE).
// tokio-util's ReaderStream defaults to 4 KiB, which means ~256 wake-ups per
// MiB of payload — wasteful on large assets like videos, wasm bundles, and
// fonts. 64 KiB roughly matches typical TCP send-buffer sizing and cuts the
// wake-up / syscall count ~16x at negligible peak-memory cost per in-flight
// body. The small/buffered path (size < STATIC_READ_MAX_SIZE) is unaffected —
// it already returns the whole body as a single Bytes.
const STREAM_CHUNK_SIZE: usize = 64 * 1024;

// Pre-built Aho-Corasick automaton over the configured HTML replacement
// pairs. Built once at startup from STATIC_HTML_REPLACE_* env vars so each
// HTML response runs a single linear scan over the body — vs the old loop
// which re-scanned and re-allocated the buffer once per (key,value) pair.
#[derive(Debug)]
pub struct HtmlReplacer {
    automaton: AhoCorasick,
    replacements: Vec<Vec<u8>>,
}

impl HtmlReplacer {
    // Returns `None` when no usable pairs are configured. Empty keys are
    // dropped on the floor — Aho-Corasick rejects (or, depending on version,
    // matches infinitely at) zero-length patterns, and an empty replacement
    // key is almost certainly a misconfiguration (e.g. `STATIC_HTML_REPLACE_=`).
    pub fn new(pairs: Vec<(Vec<u8>, Vec<u8>)>) -> Option<Self> {
        let pairs: Vec<(Vec<u8>, Vec<u8>)> =
            pairs.into_iter().filter(|(k, _)| !k.is_empty()).collect();
        if pairs.is_empty() {
            return None;
        }
        let patterns: Vec<&[u8]> = pairs.iter().map(|(k, _)| k.as_slice()).collect();
        let automaton = match AhoCorasick::new(&patterns) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "failed to build HTML replacement automaton; replacements disabled"
                );
                return None;
            }
        };
        let replacements: Vec<Vec<u8>> = pairs.into_iter().map(|(_, v)| v).collect();
        Some(Self {
            automaton,
            replacements,
        })
    }

    pub fn replace_all_bytes(&self, haystack: &[u8]) -> Vec<u8> {
        self.automaton
            .replace_all_bytes(haystack, &self.replacements)
    }
}

// Content types worth compressing — a default-deny whitelist shared with the
// on-the-fly CompressionLayer predicate (main.rs `Compressible` calls this), so
// the cached-compression path below and the layer always agree on what is
// compressible.
pub fn is_compressible_content_type(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || content_type.starts_with("application/javascript")
        || content_type.starts_with("application/json")
        || content_type.starts_with("application/manifest+json")
        || content_type.starts_with("application/xml")
        || content_type.starts_with("application/rss+xml")
        || content_type.starts_with("application/atom+xml")
        || content_type.starts_with("application/wasm")
        || content_type.starts_with("image/svg+xml")
}

// Compress a buffered body with the same encoder crate (async-compression) and
// the same 1:1 level mapping tower-http applies internally, so the cached bytes
// match what the CompressionLayer would have produced per request. Returns
// `None` on an unknown encoding or an encoder error — callers fall back to the
// identity body (the layer then compresses per request as before).
async fn compress_body(encoding: &str, level: CompressionLevel, input: &[u8]) -> Option<Bytes> {
    let level = match level {
        CompressionLevel::Fastest => Level::Fastest,
        CompressionLevel::Best => Level::Best,
        CompressionLevel::Precise(q) => Level::Precise(q),
        _ => Level::Default,
    };
    let mut out = Vec::with_capacity(input.len() / 3 + 64);
    let ok = match encoding {
        "br" => BrotliEncoder::with_quality(input, level)
            .read_to_end(&mut out)
            .await
            .is_ok(),
        "zstd" => ZstdEncoder::with_quality(input, level)
            .read_to_end(&mut out)
            .await
            .is_ok(),
        "gzip" => GzipEncoder::with_quality(input, level)
            .read_to_end(&mut out)
            .await
            .is_ok(),
        _ => false,
    };
    (ok && !out.is_empty()).then(|| Bytes::from(out))
}

// Static HTML template for directory listing view
// Includes basic styling and JavaScript for date formatting
static WEB_HTML: &str = include_str!("templates/autoindex.html");

// Escape text that is interpolated into the autoindex HTML. File names are
// attacker-influenced (uploads, third-party buckets) so an unescaped `<` or
// `"` would be a stored XSS in the directory listing.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

// Cap on the rows an autoindex listing renders. Listings are rebuilt per
// request and never cached, so an unbounded directory (100k+ entries) would
// allocate a multi-megabyte HTML body on every hit. Directories that large
// aren't human-browsable anyway — render the first N and say so.
const AUTOINDEX_MAX_ENTRIES: usize = 10_000;

async fn get_autoindex_html(path: &str) -> Result<String> {
    let entry_list = get_storage()?
        .dal
        .list(path)
        .await
        .map_err(|e| Error::Openedal { source: e })?;
    let mut html_rows =
        String::with_capacity(entry_list.len().min(AUTOINDEX_MAX_ENTRIES) * 128 + 128);
    let mut shown = 0usize;
    let mut truncated = false;
    for entry in entry_list {
        let name = entry.name();
        if name.len() <= 1 || name.starts_with('.') {
            continue;
        }
        // Hide pre-compressed siblings — they are an encoding detail, not
        // separately browsable resources.
        if name.ends_with(".br") || name.ends_with(".gz") || name.ends_with(".zst") {
            continue;
        }
        if shown == AUTOINDEX_MAX_ENTRIES {
            truncated = true;
            break;
        }
        shown += 1;

        let meta = entry.metadata();
        let mut size = String::new();
        let mut last_modified = String::new();
        if meta.is_file() {
            size = ByteSize(meta.content_length()).to_string();
            if let Some(value) = meta.last_modified() {
                last_modified = value.to_string();
            }
        }

        // href: percent-encode the (single-segment) name, preserving a
        // trailing slash for directories. display: HTML-escaped.
        let is_dir_entry = name.ends_with('/');
        let raw_name = name.trim_end_matches('/');
        let href = format!(
            "./{}{}",
            urlencoding::encode(raw_name),
            if is_dir_entry { "/" } else { "" }
        );
        let display = html_escape(name);

        let _ = write!(
            html_rows,
            r###"<tr>
                <td class="name"><a href="{href}">{display}</a></td>
                <td class="size">{size}</td>
                <td class="lastModified">{last_modified}</td>
            </tr>"###
        );
    }

    if truncated {
        let _ = write!(
            html_rows,
            r###"<tr>
                <td class="name">&hellip; listing truncated at {AUTOINDEX_MAX_ENTRIES} entries</td>
                <td class="size"></td>
                <td class="lastModified"></td>
            </tr>"###
        );
    }

    Ok(WEB_HTML.replace("{{CONTENT}}", &html_rows))
}

// The two 304 predicates, shared verbatim between `static_serve` (which builds
// the 304 response) and `load_file` (which uses them to skip the body read for
// a request that is guaranteed to 304) — sharing the exact functions is what
// makes the skip safe: the decisions cannot drift apart.
fn if_none_match_hit(if_none_match: &str, etag: &str) -> bool {
    if_none_match == "*" || if_none_match.split(',').any(|v| v.trim() == etag)
}

fn if_modified_since_hit(ims: &str, last_modified_secs: i64) -> bool {
    parse_http_date(ims)
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .is_some_and(|d| last_modified_secs <= d.as_secs() as i64)
}

// Would this request's conditional headers already guarantee a 304 against the
// given validators? Mirrors `static_serve`'s two independent checks (ETag
// first, then Last-Modified).
fn conditional_not_modified(
    params: &StaticServeParams,
    etag: Option<&str>,
    last_modified_secs: Option<i64>,
) -> bool {
    if let Some(inm) = params.if_none_match.as_deref()
        && let Some(etag) = etag
        && if_none_match_hit(inm, etag)
    {
        return true;
    }
    if let Some(ims) = params.if_modified_since.as_deref()
        && let Some(lm) = last_modified_secs
        && if_modified_since_hit(ims, lm)
    {
        return true;
    }
    false
}

// RFC 7233: a Range request guarded by `If-Range` is only honored when the
// validator still matches; otherwise the whole representation is returned.
// A weak validator MUST NOT be used here, so weak ETags never satisfy it.
fn if_range_satisfied(if_range: &str, etag: Option<&str>, last_modified_secs: Option<i64>) -> bool {
    let v = if_range.trim();
    if v.is_empty() || v.starts_with("W/") {
        return false;
    }
    if v.starts_with('"') {
        return matches!(etag, Some(e) if !e.starts_with("W/") && e == v);
    }
    match (parse_http_date(v), last_modified_secs) {
        (Ok(t), Some(lm)) => t
            .duration_since(UNIX_EPOCH)
            .is_ok_and(|d| d.as_secs() as i64 >= lm),
        _ => false,
    }
}

// Parsed `Accept-Encoding` header — built once per request and consulted O(1)
// per candidate encoding. Replaces a previous `encoding_accepted(accept,
// target)` helper that re-parsed the whole header for each candidate (called
// 3x per request for the br/zstd/gzip lookup).
//
// Quality-value aware: an explicit `br;q=0` is a refusal of brotli and does
// NOT fall back to the `*` wildcard. Only an encoding that was never mentioned
// inherits the wildcard's q value. Encoding names are matched case-insensitively
// (RFC 7231); `*` is a single literal character so case doesn't apply.
#[derive(Default)]
struct EncodingPrefs {
    br: Option<f32>,
    zstd: Option<f32>,
    gzip: Option<f32>,
    wildcard: Option<f32>,
}

impl EncodingPrefs {
    fn parse(accept: &str) -> Self {
        let mut prefs = Self::default();
        for part in accept.split(',') {
            let mut it = part.split(';');
            let name = it.next().unwrap_or("").trim();
            let mut q = 1.0f32;
            for p in it {
                if let Some(qs) = p.trim().strip_prefix("q=") {
                    q = qs.parse().unwrap_or(0.0);
                }
            }
            let slot: &mut Option<f32> = if name.eq_ignore_ascii_case("br") {
                &mut prefs.br
            } else if name.eq_ignore_ascii_case("zstd") {
                &mut prefs.zstd
            } else if name.eq_ignore_ascii_case("gzip") {
                &mut prefs.gzip
            } else if name == "*" {
                &mut prefs.wildcard
            } else {
                continue;
            };
            // First-occurrence wins — matches the prior early-return loop's
            // behavior on repeated tokens (pathological in practice).
            slot.get_or_insert(q);
        }
        prefs
    }

    fn accepts(&self, encoding: &str) -> bool {
        let explicit = match encoding {
            "br" => self.br,
            "zstd" => self.zstd,
            "gzip" => self.gzip,
            _ => None,
        };
        match explicit {
            Some(q) => q > 0.0,
            None => self.wildcard.is_some_and(|q| q > 0.0),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct StaticServeParams {
    pub file: String,
    pub index: Arc<str>,
    pub autoindex: bool,
    pub cache_control: Arc<str>,
    pub cache_control_map: Arc<HashMap<String, String>>,
    pub html_replacer: Option<Arc<HtmlReplacer>>,
    pub cache_size: usize,
    pub cache_ttl: Duration,
    // Opt-in short TTLs (zero = disabled, capped at 5 minutes by config
    // validation): negative caching of 404 lookups, and in-memory caching of
    // HTML bodies (which clients still receive as `no-cache` — only the
    // backend round-trips are amortized).
    pub not_found_cache_ttl: Duration,
    pub html_cache_ttl: Duration,
    // Per-request header echoes — Arc<str> so the fallback retry loop in
    // main.rs can clone/share them at refcount cost instead of allocating
    // fresh String copies per iteration.
    pub range: Option<Arc<str>>,
    pub if_range: Option<Arc<str>>,
    pub if_none_match: Option<Arc<str>>,
    pub if_modified_since: Option<Arc<str>>,
    pub accept_encoding: Option<Arc<str>>,
    // Whether to probe `.br`/`.zst`/`.gz` sibling files (STATIC_PRECOMPRESSED).
    // Kept separate from `accept_encoding` presence: cached dynamic compression
    // also needs the Accept-Encoding header, but must not trigger sibling
    // `stat` probes for files that were never pre-compressed.
    pub precompressed: bool,
    // Cached dynamic compression (STATIC_COMPRESS_CACHE): compress cacheable
    // buffered bodies once with the CompressionLayer's encoders/level and store
    // them under the encoding-aware cache key.
    pub compress_cache: bool,
    pub compress_min_length: u64,
    pub compress_level: CompressionLevel,
    pub read_max_size: u64,
    pub head: bool,
    pub request_path: Arc<str>,
    pub request_query: Option<Arc<str>>,
}

#[derive(Clone)]
struct FileInfoCache {
    expired_at: u64,
    data: CacheValue,
}

// A cache entry is either a real file or a remembered 404. Negative entries
// exist only when STATIC_NOT_FOUND_CACHE_TTL > 0 and live under the identity
// (empty-encoding) key — a 404 does not vary by Accept-Encoding. `Found` is
// Arc-shared so cache hits cost a refcount bump, not a deep clone of
// `headers` (Vec) + `read_file` (String). Bytes is already cheap-clone.
#[derive(Clone)]
enum CacheValue {
    Found(Arc<FileInfo>),
    NotFound,
}

struct FileInfo {
    headers: Vec<(HeaderName, HeaderValue)>,
    body: Option<Bytes>,
    size: u64,
    read_file: String,
    last_modified_secs: Option<i64>,
}

static STATIC_CACHE: OnceLock<TinyUfo<String, FileInfoCache>> = OnceLock::new();

fn get_static_cache(size: usize) -> &'static TinyUfo<String, FileInfoCache> {
    STATIC_CACHE.get_or_init(|| TinyUfo::new(size, size))
}

// Cache keys are encoding-aware: the same logical path is stored once per
// negotiated Content-Encoding (identity uses an empty suffix). This lets
// pre-compressed `.br`/`.zst`/`.gz` responses be cached safely — a key encodes
// exactly which bytes/encoding it holds — and lets a hit skip the path
// validation + `stat` + sibling-probe round-trips entirely. The separator is a
// NUL byte, which never appears in a normal (url-decoded) path segment.
fn encoding_cache_key(file: &str, encoding: &str) -> String {
    let mut key = String::with_capacity(file.len() + 1 + encoding.len());
    key.push_str(file);
    key.push('\u{0}');
    key.push_str(encoding);
    key
}

// Seconds since the Unix epoch. Read once per request at the `get_file` cache
// probe and threaded through, so a request that probes several encoding keys
// does not re-read the clock for each (up to 4 reads collapse to 1).
fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn get_file_from_cache(file: &String, cache_size: usize, now_secs: u64) -> Option<CacheValue> {
    if let Some(info) = get_static_cache(cache_size).get(file)
        && info.expired_at > now_secs
    {
        return Some(info.data.clone());
    }
    None
}

// Translate a cache hit into the `get_file` result: a positive entry returns
// the shared FileInfo, a negative entry replays the remembered 404.
fn cached_result(hit: CacheValue, file: &str) -> Result<Arc<FileInfo>> {
    match hit {
        CacheValue::Found(info) => Ok(info),
        CacheValue::NotFound => Err(Error::NotFound {
            file: file.to_string(),
        }),
    }
}

fn set_file_to_cache(key: String, value: CacheValue, cache_size: usize, cache_ttl: Duration) {
    let expired_at = now_unix_secs() + cache_ttl.as_secs();
    let cache = get_static_cache(cache_size);
    // The caller already built the owned key (`encoding_cache_key`); take it by
    // value so we don't clone it again here.
    // Is this a physically new key, or a re-cache of a still-present but
    // logically-expired entry? The occupancy gauge counts distinct entries, so
    // only a genuinely new key adds one. `put` returns the entries it evicted to
    // make room; subtract those. The pre-`put` lookup exists solely to feed the
    // gauge, so skip it when metrics are off (short-circuits before the `get`).
    let is_new = metrics::enabled() && cache.get(&key).is_none();
    let evicted = cache.put(
        key,
        FileInfoCache {
            expired_at,
            data: value,
        },
        1,
    );
    metrics::record_cache_insert(is_new, evicted.len());
}

// Negative-cache a 404 (opt-in via STATIC_NOT_FOUND_CACHE_TTL). Keyed by the
// request path under the identity encoding so every later lookup — whatever
// its Accept-Encoding — hits it on the identity probe and skips the backend
// `stat` entirely. Only genuine not-found results are stored; other errors
// (and the request-specific 301 redirect) are never cached.
fn maybe_cache_not_found(params: &StaticServeParams, res: &Result<Arc<FileInfo>>) {
    if params.cache_size == 0 || params.not_found_cache_ttl.is_zero() {
        return;
    }
    if let Err(e) = res
        && e.is_not_found()
    {
        let key = encoding_cache_key(&params.file, "");
        set_file_to_cache(
            key,
            CacheValue::NotFound,
            params.cache_size,
            params.not_found_cache_ttl,
        );
    }
}

// Single-flight coordination. A burst of concurrent cache misses for the same
// logical request would each independently `stat`/`read` the backend; this
// collapses them so exactly one (the "leader") loads while the rest ("followers")
// share its result. The shared result is the immutable `Arc<FileInfo>` — for
// streamed files (`body == None`) each follower still opens its own byte stream
// from `read_file` in `static_serve`, so only the metadata load is deduplicated.
struct Flight {
    done: Notify,
    result: Mutex<Option<Arc<FileInfo>>>,
}

impl Flight {
    fn new() -> Self {
        Self {
            done: Notify::new(),
            result: Mutex::new(None),
        }
    }
    fn result(&self) -> Option<Arc<FileInfo>> {
        self.result
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn publish(&self, info: Arc<FileInfo>) {
        *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(info);
    }
}

static INFLIGHT: OnceLock<Mutex<HashMap<String, Arc<Flight>>>> = OnceLock::new();

fn inflight() -> &'static Mutex<HashMap<String, Arc<Flight>>> {
    INFLIGHT.get_or_init(|| Mutex::new(HashMap::new()))
}

// Leader-side RAII cleanup for a single-flight slot. The leader's future can be
// dropped mid-load (client disconnected, hyper cancels the handler); without
// the guard the key would stay in INFLIGHT forever and every later request for
// it would follow a dead flight, hanging until the request timeout. Dropping
// the guard removes the slot and wakes any followers — on cancellation no
// result was published, so they fall through to an independent load.
struct FlightGuard {
    key: String,
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        if let Some(flight) = inflight()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key)
        {
            flight.done.notify_waiters();
        }
    }
}

// Two requests resolve to byte-identical responses iff they share the same
// logical path, the same set of accepted compression encodings (negotiation is
// purely a boolean per encoding), and the same HEAD-ness (HEAD has no body).
// Those three inputs form the single-flight key. The separator is a NUL byte.
fn singleflight_key(file: &str, prefs: &Option<EncodingPrefs>, head: bool) -> String {
    let (br, zstd, gzip) = match prefs {
        Some(p) => (p.accepts("br"), p.accepts("zstd"), p.accepts("gzip")),
        None => (false, false, false),
    };
    format!(
        "{file}\u{0}{}{}{}{}",
        br as u8, zstd as u8, gzip as u8, head as u8
    )
}

async fn get_file(params: &StaticServeParams) -> Result<Arc<FileInfo>> {
    let file = params.file.clone();

    // Parse Accept-Encoding once: reused for both the encoding-aware cache
    // lookup below and pre-compressed sibling negotiation further down.
    let accept_prefs = params.accept_encoding.as_deref().map(EncodingPrefs::parse);

    // Encoding-aware cache lookup. Probe each encoding the client accepts in
    // priority order (br > zstd > gzip), then identity. The first hit returns a
    // fully built FileInfo (correct Content-Encoding + body) and skips the
    // validate / `stat` / sibling-probe round-trips below. Probing a variant
    // that was never cached is a cheap in-memory miss, so unsupported paths
    // (e.g. HTML, which is never cached) simply fall through to the slow path.
    if params.cache_size > 0 {
        // Reuse a single key buffer across every probe instead of allocating a
        // fresh String per encoding (up to 4 per request). The `<path>\0` prefix
        // is constant; only the encoding suffix changes, so truncate back to it
        // and rewrite. Capacity covers the longest suffix ("zstd"/"gzip") so no
        // probe regrows the buffer.
        let mut key = String::with_capacity(file.len() + 1 + 4);
        key.push_str(&file);
        key.push('\u{0}');
        let prefix_len = key.len();
        // Read the clock once for the whole probe sequence: every key shares the
        // same expiry cutoff, and the probes run within microseconds.
        let now_secs = now_unix_secs();
        if let Some(prefs) = &accept_prefs {
            for enc in ["br", "zstd", "gzip"] {
                if prefs.accepts(enc) {
                    key.truncate(prefix_len);
                    key.push_str(enc);
                    if let Some(hit) = get_file_from_cache(&key, params.cache_size, now_secs) {
                        metrics::record_cache_hit();
                        return cached_result(hit, &file);
                    }
                }
            }
        }
        // identity: empty suffix (back to just `<path>\0`). Negative (404)
        // entries live only under this key, so they are reached regardless of
        // what the client accepts.
        key.truncate(prefix_len);
        if let Some(hit) = get_file_from_cache(&key, params.cache_size, now_secs) {
            metrics::record_cache_hit();
            return cached_result(hit, &file);
        }
        metrics::record_cache_miss();
    }

    // Single-flight dispatch: become the leader for this key, or follow an
    // already in-flight leader and share its result.
    let sf_key = singleflight_key(&file, &accept_prefs, params.head);
    let follow = {
        let mut map = inflight().lock().unwrap_or_else(|e| e.into_inner());
        match map.get(&sf_key) {
            Some(flight) => Some(flight.clone()),
            None => {
                map.insert(sf_key.clone(), Arc::new(Flight::new()));
                None
            }
        }
    };
    if let Some(flight) = follow {
        // Follower: register for the wake-up *before* checking the result so a
        // notify firing in between is not lost, then await it. A missing/empty
        // result (leader errored or produced an uncacheable response) falls
        // through to an independent load. The miss was already counted above.
        let notified = flight.done.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(info) = flight.result() {
            return Ok(info);
        }
        notified.await;
        if let Some(info) = flight.result() {
            return Ok(info);
        }
        let res = load_file(params, file, &accept_prefs)
            .await
            .map(|(info, _)| info);
        maybe_cache_not_found(params, &res);
        return res;
    }

    // Leader: load, publish the result to any followers, then clear the slot.
    // The guard owns the remove + wake so they also run if this future is
    // dropped mid-load (client disconnect) — a cancelled leader must not leave
    // a dead flight behind. Publish happens before the guard drops, so woken
    // followers always observe the result.
    let guard = FlightGuard { key: sf_key };
    let res = load_file(params, file, &accept_prefs).await;
    if let Ok((info, share)) = &res
        && *share
    {
        // `share == false` means the body read was skipped for a guaranteed
        // 304 — publishing that would hand followers a body-less FileInfo they
        // would stream raw (bypassing buffering and the HTML replacer). They
        // fall through to an independent load instead.
        let flight = inflight()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&guard.key)
            .cloned();
        if let Some(flight) = flight {
            flight.publish(info.clone());
        }
    }
    let res = res.map(|(info, _)| info);
    maybe_cache_not_found(params, &res);
    res
}

// Join a directory path and an index filename, inserting a single '/' only when
// the directory part lacks a trailing one. Shared by the optimistic index probe
// and the directory fallback so both target byte-identical backend keys (the
// empty/root dir yields a leading-slash `/index.html`, matching the historical
// join).
fn join_index(dir: &str, index: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{index}")
    } else {
        format!("{dir}/{index}")
    }
}

// Returns the loaded file plus a `share` flag: false when the body read was
// skipped because the request's conditionals guarantee a 304 — such a result
// is valid only for THIS request and must not be published to single-flight
// followers or cached (both enforced by the callers/`will_cache`).
async fn load_file(
    params: &StaticServeParams,
    mut file: String,
    accept_prefs: &Option<EncodingPrefs>,
) -> Result<(Arc<FileInfo>, bool)> {
    let storage = get_storage()?;
    storage.validate(&file)?;

    // Optimistic index resolution. For a trailing-slash directory request with
    // autoindex off and an index file configured, `<dir>/<index>` almost always
    // exists and the directory's own metadata is then thrown away. Stat the
    // index directly: on a hit we skip the separate directory stat entirely (one
    // backend round-trip instead of two — significant on remote backends like
    // S3/FTP/GridFS, negligible on local FS). The trailing-slash guard is also a
    // correctness condition: a directory reached *without* the slash must 301
    // first (below), which requires stat'ing the directory itself. On a NotFound
    // miss we fall back to the normal directory stat and skip re-probing the
    // index (`index_missing`); a non-NotFound error is the only servable target
    // failing, so it propagates rather than masking a 5xx as a 404.
    let mut index_meta = None;
    let mut index_missing = false;
    if params.request_path.ends_with('/') && !params.autoindex && !params.index.is_empty() {
        let index_path = join_index(&file, &params.index);
        match storage.dal.stat(&index_path).await {
            Ok(m) if m.is_file() => {
                file = index_path;
                index_meta = Some(m);
            }
            // Index path is itself a directory (rare): fall back to the normal
            // directory handling below, which resolves it as it does today.
            Ok(_) => {}
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => index_missing = true,
            Err(e) => return Err(Error::Openedal { source: e }),
        }
    }

    let mut meta = match index_meta {
        Some(m) => m,
        None => storage
            .dal
            .stat(&file)
            .await
            .map_err(|e| Error::Openedal { source: e })?,
    };

    let is_dir = meta.is_dir();
    if is_dir && !params.autoindex && params.index.is_empty() {
        return Err(Error::NotFound { file: file.clone() });
    }
    // Directory served without a trailing slash: 301 to add it so that
    // relative URLs in the served page resolve correctly.
    if is_dir && !params.request_path.ends_with('/') {
        let mut location = format!("{}/", params.request_path);
        if let Some(query) = params.request_query.as_deref() {
            location.push('?');
            location.push_str(query);
        }
        return Err(Error::MovedPermanently { location });
    }
    // The file path pushes up to 9 headers (Accept-Ranges, Cache-Control,
    // Content-Type, Content-Encoding, Vary, ETag, Last-Modified, X-Original-Size,
    // Content-Length); size to 10 so it never reallocates.
    let mut headers = Vec::with_capacity(10);
    headers.push((header::ACCEPT_RANGES, HeaderValue::from_static("bytes")));

    if is_dir && params.autoindex {
        let html = get_autoindex_html(&file).await?;
        headers.push((header::CONTENT_TYPE, HeaderValue::from_static("text/html")));
        headers.push((header::CACHE_CONTROL, HeaderValue::from_static("no-cache")));
        headers.push((header::VARY, HeaderValue::from_static("Accept-Encoding")));
        let body = Bytes::from(html);
        // Directory listings are mutable (entries change between requests) and
        // are `no-cache` like HTML — never store them in the in-memory cache.
        let info = FileInfo {
            size: body.len() as u64,
            headers,
            body: Some(body),
            read_file: file.clone(),
            last_modified_secs: None,
        };
        return Ok((Arc::new(info), true));
    }
    if is_dir && !params.index.is_empty() {
        // The optimistic probe above already found the index absent — the
        // directory exists but has nothing servable (autoindex is off here), so
        // 404 directly instead of stat'ing the same missing index a second time.
        if index_missing {
            return Err(Error::NotFound { file: file.clone() });
        }
        file = join_index(&file, &params.index);
        meta = storage
            .dal
            .stat(&file)
            .await
            .map_err(|e| Error::Openedal { source: e })?;
    }
    let content_type = meta
        .content_type()
        .map(|v| v.to_string())
        .unwrap_or_else(|| {
            mime_guess::from_path(Path::new(&file))
                .first_or_octet_stream()
                .to_string()
        });
    // Web-critical types that some MIME databases miss. Only fill in when the
    // type could not be determined (missing or generic octet-stream) so an
    // explicit type set by the storage backend still wins.
    let content_type = if content_type.is_empty() || content_type == "application/octet-stream" {
        match Path::new(&file)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("wasm") => "application/wasm".to_string(),
            Some("mjs") => "text/javascript".to_string(),
            _ => content_type,
        }
    } else {
        content_type
    };
    let mut is_html = false;
    let cache_control: String = if content_type.contains("text/html") {
        is_html = true;
        "no-cache".to_string()
    } else if let Some(cc) = meta.cache_control() {
        cc.to_string()
    } else {
        // Check per-extension override before falling back to global default
        let ext = Path::new(&file)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase());
        if let Some(ext) = ext
            && let Some(cc) = params.cache_control_map.get(&ext)
        {
            cc.clone()
        } else {
            params.cache_control.to_string()
        }
    };
    if let Ok(v) = HeaderValue::try_from(cache_control) {
        headers.push((header::CACHE_CONTROL, v));
    }
    let compressible = is_compressible_content_type(&content_type);
    if let Ok(v) = HeaderValue::try_from(content_type) {
        headers.push((header::CONTENT_TYPE, v));
    }
    // Try pre-compressed file (.br / .zst / .gz) if enabled and client supports
    // it. `negotiated_encoding` is the encoding actually served ("" = identity)
    // and becomes the cache-key suffix below. Priority: brotli > zstd > gzip.
    // Gated on `precompressed`, not on Accept-Encoding presence — compress_cache
    // also populates accept_prefs, and must not pay sibling `stat` probes for
    // files that were never pre-compressed.
    let mut precompressed_file = None;
    let mut negotiated_encoding: &'static str = "";
    if params.precompressed
        && let Some(prefs) = accept_prefs
        && !is_html
        && !is_dir
    {
        let candidates: &[(&str, &str)] = &[("br", ".br"), ("zstd", ".zst"), ("gzip", ".gz")];
        for (encoding, ext) in candidates {
            if prefs.accepts(encoding) {
                let compressed = format!("{file}{ext}");
                if let Ok(compressed_meta) = storage.dal.stat(&compressed).await {
                    precompressed_file = Some(compressed);
                    meta = compressed_meta;
                    headers.push((header::CONTENT_ENCODING, HeaderValue::from_static(encoding)));
                    negotiated_encoding = encoding;
                    break;
                }
            }
        }
    }
    if precompressed_file.is_none()
        && let Some(content_encoding) = meta.content_encoding()
        && let Ok(v) = HeaderValue::try_from(content_encoding.to_string())
    {
        headers.push((header::CONTENT_ENCODING, v));
    }
    // The global CompressionLayer may compress this response on the fly based
    // on the client's Accept-Encoding (and `.br`/`.zst`/`.gz` negotiation
    // above also varies by it), so always advertise the variance — otherwise a
    // shared cache can hand a compressed body to a client that can't decode it.
    headers.push((header::VARY, HeaderValue::from_static("Accept-Encoding")));

    let size = meta.content_length();
    // Extract last_modified once so it can be used for both ETag and Last-Modified header
    let last_modified_ms = meta
        .last_modified()
        .map(|lm| lm.into_inner().as_millisecond())
        .filter(|&ms| ms > 0);
    let etag = if let Some(etag) = meta.etag() {
        Some(etag.to_string())
    } else {
        last_modified_ms.map(|ms| format!(r#"W/"{size:x}-{ms:x}""#))
    };
    if let Some(etag) = &etag
        && let Ok(v) = HeaderValue::try_from(etag.as_str())
    {
        headers.push((header::ETAG, v));
    }
    let last_modified_secs = last_modified_ms.map(|ms| ms / 1000);
    if let Some(secs) = last_modified_secs {
        let sys_time = UNIX_EPOCH + Duration::from_secs(secs as u64);
        if let Ok(v) = HeaderValue::try_from(fmt_http_date(sys_time)) {
            headers.push((header::LAST_MODIFIED, v));
        }
    }

    // size.to_string() is decimal digits — always a valid HeaderValue.
    // X-Original-Size is the stat (pre-compression) size; Content-Length is
    // pushed after the body is known, because HTML replacement and cached
    // compression can change the served byte count.
    if let Ok(v) = HeaderValue::from_str(&size.to_string()) {
        headers.push((X_ORIGINAL_SIZE_HEADER_NAME.clone(), v));
    }

    // Will this load's result be stored in the in-memory cache? HTML caches
    // only when STATIC_HTML_CACHE_TTL opted in; everything else follows
    // cache_ttl. Shared by the conditional short-circuit below and the store at
    // the end of the function.
    let entry_ttl = if is_html {
        params.html_cache_ttl
    } else {
        params.cache_ttl
    };
    let will_cache = params.cache_size > 0 && !params.head && !entry_ttl.is_zero();

    // Conditional short-circuit: when the request's validators already
    // guarantee a 304 (same predicate functions static_serve re-checks against
    // the returned headers) and this entry would not be cached anyway, reading
    // the body would be pure waste — the bytes are dropped on the 304 path.
    // This matters for HTML, which is uncached by default: every revalidation
    // would otherwise re-read the full body from the backend. When the entry
    // WILL be cached the body is read regardless, since it amortizes across
    // future requests. A body skipped this way must never be published to
    // single-flight followers (share = false below): a non-conditional
    // follower would stream it raw, bypassing buffering and the HTML replacer.
    let not_modified =
        !will_cache && conditional_not_modified(params, etag.as_deref(), last_modified_secs);

    // read html or small file
    let read_file = precompressed_file.as_deref().unwrap_or(&file);
    let mut body = if !params.head && !not_modified && (is_html || size < params.read_max_size) {
        let buffer = storage
            .dal
            .read(read_file)
            .await
            .map_err(|e| Error::Openedal { source: e })?;

        // Only apply HTML replacements to HTML content. Single pass: the
        // Aho-Corasick automaton walks the body once and emits one new Vec,
        // regardless of how many (key, value) pairs are configured.
        if is_html && let Some(replacer) = &params.html_replacer {
            Some(Bytes::from(replacer.replace_all_bytes(&buffer.to_bytes())))
        } else {
            // `Buffer::to_bytes` is zero-copy when the backend returned one
            // contiguous chunk (the common case for a single read) — unlike the
            // previous `to_vec()`, which always copied the whole body.
            Some(buffer.to_bytes())
        }
    } else {
        None
    };

    // Cached dynamic compression (STATIC_COMPRESS_CACHE): compress a cacheable
    // buffered body once — with the same encoder crate and level mapping the
    // CompressionLayer applies per request — and let it be stored under the
    // encoding-aware cache key below, so later requests serve the compressed
    // bytes directly instead of re-compressing per response. Restricted to
    // entries that will actually be cached (otherwise this just duplicates the
    // layer's work) and representations that aren't already encoded (a
    // pre-compressed sibling or a backend-set Content-Encoding wins). The
    // resulting Content-Encoding header makes the CompressionLayer skip this
    // response.
    if params.compress_cache
        && will_cache
        && negotiated_encoding.is_empty()
        && compressible
        && !headers.iter().any(|(k, _)| *k == header::CONTENT_ENCODING)
        && let Some(prefs) = accept_prefs
        && let Some(b) = &body
        && b.len() >= params.compress_min_length as usize
    {
        // Best accepted encoding only — each encoding is its own cache entry,
        // so a client with different preferences populates its own variant.
        for enc in ["br", "zstd", "gzip"] {
            if prefs.accepts(enc) {
                // Keep identity when compression doesn't actually shrink the
                // body — an entry that is no smaller isn't worth the decode.
                if let Some(compressed) = compress_body(enc, params.compress_level, b).await
                    && compressed.len() < b.len()
                {
                    headers.push((header::CONTENT_ENCODING, HeaderValue::from_static(enc)));
                    negotiated_encoding = enc;
                    body = Some(compressed);
                }
                break;
            }
        }
    }

    // Content-Length reflects the bytes actually served: the (possibly
    // replaced/compressed) buffered body, or the stat size for streamed files.
    let content_length = body.as_ref().map(|b| b.len() as u64).unwrap_or(size);
    if let Ok(v) = HeaderValue::from_str(&content_length.to_string()) {
        headers.push((header::CONTENT_LENGTH, v));
    }

    // Cache under the encoding-aware key (`negotiated_encoding` is "" for
    // identity, or the pre-compressed encoding). Because the key now encodes
    // exactly which bytes it holds, pre-compressed responses are safe to cache
    // — a differently-negotiating client probes a different key. Metadata-only
    // entries (large/streamed files, `body == None`) are cached too: a hit then
    // skips the `stat` + sibling probes and streams straight from `read_file`.
    // HEAD requests are never cached (their body is always None even for small
    // files, which would force a later GET onto the streaming path).
    let read_path = precompressed_file.unwrap_or_else(|| file.clone());
    let info = Arc::new(FileInfo {
        headers,
        body,
        size,
        read_file: read_path,
        last_modified_secs,
    });
    if will_cache {
        // HTML is cached only when STATIC_HTML_CACHE_TTL opted in (clients
        // still see `no-cache`; this only amortizes backend reads over a short,
        // bounded window). Everything else keeps the regular TTL. Key by the
        // *request* path (`params.file`), not the possibly index-joined `file`
        // — lookups probe the request path, so a directory request must store
        // under the key it will probe next time. `will_cache` implies
        // `!not_modified`, so a body-skipped conditional load is never stored.
        let key = encoding_cache_key(&params.file, negotiated_encoding);
        set_file_to_cache(
            key,
            CacheValue::Found(info.clone()),
            params.cache_size,
            entry_ttl,
        );
    }

    Ok((info, !not_modified))
}

// Cached result of the custom `404.html` lookup: `Some(body)` when the page
// exists, `None` when the backend definitively reported NotFound. Without this
// every 404 pays a full backend read — or a failed-read round trip when the
// file is absent — so on remote backends a path-scanning bot makes 404s more
// expensive than real files. A single global slot: the path is fixed, so there
// is nothing to key by.
static NOT_FOUND_PAGE: Mutex<Option<(u64, Option<Bytes>)>> = Mutex::new(None);

fn cached_not_found_page(now_secs: u64) -> Option<Option<Bytes>> {
    let slot = NOT_FOUND_PAGE.lock().unwrap_or_else(|e| e.into_inner());
    match &*slot {
        Some((expires, result)) if *expires > now_secs => Some(result.clone()),
        _ => None,
    }
}

fn store_not_found_page(result: Option<Bytes>, ttl: Duration) {
    *NOT_FOUND_PAGE.lock().unwrap_or_else(|e| e.into_inner()) =
        Some((now_unix_secs() + ttl.as_secs(), result));
}

// Fetch the custom 404 page (`404.html` in the storage root), caching both the
// body and its absence for `cache_ttl` (zero = uncached, the historical
// behavior). Only definitive outcomes are cached — a transient backend error
// yields an uncached `None` so the page is not remembered as missing for a
// whole TTL. The caller passes max(not_found_cache_ttl, html_cache_ttl): both
// are the existing ≤5-minute bounded-staleness opt-ins, and opting into either
// means accepting a short window of 404-page staleness.
pub async fn not_found_page(cache_ttl: Duration) -> Option<Bytes> {
    if !cache_ttl.is_zero()
        && let Some(cached) = cached_not_found_page(now_unix_secs())
    {
        return cached;
    }
    let Ok(storage) = get_storage() else {
        return None;
    };
    match storage.dal.read("404.html").await {
        Ok(buf) => {
            let body = Some(buf.to_bytes());
            if !cache_ttl.is_zero() {
                store_not_found_page(body.clone(), cache_ttl);
            }
            body
        }
        Err(e) => {
            if e.kind() == opendal::ErrorKind::NotFound && !cache_ttl.is_zero() {
                store_not_found_page(None, cache_ttl);
            }
            None
        }
    }
}

// A parsed `Range` header. `Ranges` holds one or more satisfiable byte ranges
// (inclusive, clamped to the representation) in request order: a single entry
// yields a `206` with `Content-Range`, multiple entries a `multipart/byteranges`
// body.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RangesValue {
    NotSatisfiable,
    Ranges(Vec<(u64, u64)>),
}

#[derive(Debug, PartialEq, Eq)]
enum OneRange {
    Satisfiable(u64, u64),
    Unsatisfiable,
}

// Cap on ranges accepted in a single request. A client can otherwise amplify one
// request into thousands of tiny reads / multipart parts; beyond this we ignore
// the Range header entirely and serve the full 200.
const MAX_RANGES: usize = 100;

// Cap on the total bytes a multipart/byteranges response may cover. The
// single-range path streams, but multipart is assembled in memory, and ranges
// may overlap — so MAX_RANGES alone only bounds the part *count*: 100
// full-file ranges over a 10 GB file would try to buffer ~1 TB. Multi-range
// requests are in practice small seeks (media indexes, PDF chunks); past this
// cap the Range header is ignored and the full representation is served as a
// (streaming) 200. Single ranges are exempt — they never buffer.
const MAX_MULTIPART_TOTAL_BYTES: u64 = 8 * 1024 * 1024;

// Parse a single `start-end` spec against the representation size. `None` means
// the spec is syntactically malformed — per RFC 7233 the caller then ignores the
// whole Range header and serves the full representation.
fn parse_one_range(spec: &str, total_size: u64) -> Option<OneRange> {
    let (start_str, end_str) = spec.split_once('-')?;
    let start_str = start_str.trim();
    let end_str = end_str.trim();

    if total_size == 0 {
        return Some(OneRange::Unsatisfiable);
    }

    let (start, end) = if start_str.is_empty() {
        // bytes=-500 (suffix length)
        let suffix_len: u64 = end_str.parse().ok()?;
        if suffix_len == 0 {
            return Some(OneRange::Unsatisfiable);
        }
        if suffix_len >= total_size {
            (0, total_size - 1)
        } else {
            (total_size - suffix_len, total_size - 1)
        }
    } else if end_str.is_empty() {
        // bytes=500-
        let start: u64 = start_str.parse().ok()?;
        if start >= total_size {
            return Some(OneRange::Unsatisfiable);
        }
        (start, total_size - 1)
    } else {
        // bytes=500-999
        let start: u64 = start_str.parse().ok()?;
        let end: u64 = end_str.parse().ok()?;
        if start > end {
            return None; // malformed
        }
        if start >= total_size {
            return Some(OneRange::Unsatisfiable);
        }
        (start, end.min(total_size - 1))
    };

    Some(OneRange::Satisfiable(start, end))
}

// Parse a full `Range` header (`bytes=a-b,c-d,...`). Returns `None` when the
// header is absent/unparsable or carries too many ranges (→ full 200),
// `NotSatisfiable` when every range is unsatisfiable (→ 416), or the satisfiable
// ranges in request order. A single satisfiable range is the common single-206
// case; unsatisfiable members of an otherwise-satisfiable set are dropped (RFC
// 7233 §4.1).
fn parse_ranges(range_header: &str, total_size: u64) -> Option<RangesValue> {
    let range_str = range_header.strip_prefix("bytes=")?;
    let mut satisfiable: Vec<(u64, u64)> = Vec::new();
    let mut count = 0usize;
    for spec in range_str.split(',') {
        let spec = spec.trim();
        if spec.is_empty() {
            continue;
        }
        count += 1;
        if count > MAX_RANGES {
            return None;
        }
        match parse_one_range(spec, total_size)? {
            OneRange::Satisfiable(start, end) => satisfiable.push((start, end)),
            OneRange::Unsatisfiable => {}
        }
    }
    if count == 0 {
        return None;
    }
    if satisfiable.is_empty() {
        return Some(RangesValue::NotSatisfiable);
    }
    // Multi-range only: bound the bytes the in-memory multipart assembly would
    // buffer (ranges may overlap, so this can exceed the representation size).
    if satisfiable.len() > 1 {
        let total: u64 = satisfiable
            .iter()
            .fold(0u64, |acc, &(s, e)| acc.saturating_add(e - s + 1));
        if total > MAX_MULTIPART_TOTAL_BYTES {
            return None;
        }
    }
    Some(RangesValue::Ranges(satisfiable))
}

// Monotonic counter feeding the multipart boundary token. Combined with the
// representation size it makes a boundary unique per response and vanishingly
// unlikely to collide with the payload bytes.
static MULTIPART_BOUNDARY_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_multipart_boundary(total_size: u64) -> String {
    let n = MULTIPART_BOUNDARY_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("static_serve_{total_size:x}_{n:016x}")
}

// Assemble a `multipart/byteranges` body in memory. Each part carries a boundary
// line plus `Content-Type`/`Content-Range` headers, then the range bytes; the
// epilogue is the closing `--boundary--`. Buffered bodies are sliced directly;
// streamed (large) files read each range from the backend. Unlike the single-
// range path this buffers the requested bytes rather than streaming — multi-range
// is used almost exclusively for small seeks (media, PDFs) and is bounded by
// MAX_RANGES (part count) and MAX_MULTIPART_TOTAL_BYTES (total payload).
async fn build_multipart_byteranges(
    file_info: &FileInfo,
    ranges: &[(u64, u64)],
    total_size: u64,
    boundary: &str,
) -> Result<Bytes> {
    let content_type = file_info
        .headers
        .iter()
        .find(|(k, _)| *k == header::CONTENT_TYPE)
        .and_then(|(_, v)| v.to_str().ok())
        .unwrap_or("application/octet-stream");

    // Pre-size the buffer: exact range bytes plus a per-part header/boundary
    // estimate and the closing boundary, so the common small-range case grows
    // the Vec at most once instead of reallocating per part. ~80 covers each
    // part's fixed literals ("--", the two header field names, the CRLFs) and
    // the decimal digits of start/end/total_size; an over-estimate is harmless.
    let data_len: usize = ranges.iter().map(|&(s, e)| (e - s + 1) as usize).sum();
    let estimated =
        data_len + ranges.len() * (boundary.len() + content_type.len() + 80) + boundary.len() + 8;
    let mut buf: Vec<u8> = Vec::with_capacity(estimated);
    for &(start, end) in ranges {
        let part_header = format!(
            "--{boundary}\r\nContent-Type: {content_type}\r\nContent-Range: bytes {start}-{end}/{total_size}\r\n\r\n"
        );
        buf.extend_from_slice(part_header.as_bytes());
        if let Some(body) = file_info.body.as_ref() {
            buf.extend_from_slice(&body[start as usize..=end as usize]);
        } else {
            let chunk = get_storage()?
                .dal
                .read_with(&file_info.read_file)
                .range(start..=end)
                .await
                .map_err(|e| Error::Openedal { source: e })?;
            // `to_bytes()` is zero-copy on a contiguous backend chunk, so this
            // is a single copy into `buf` — vs `to_vec()`, which allocated a
            // throwaway Vec and copied twice.
            buf.extend_from_slice(&chunk.to_bytes());
        }
        buf.extend_from_slice(b"\r\n");
    }
    buf.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Ok(Bytes::from(buf))
}

// 处理函数
pub async fn static_serve(params: &StaticServeParams) -> Result<Response> {
    let file_info = get_file(params).await?;
    let total_size = file_info
        .body
        .as_ref()
        .map(|b| b.len() as u64)
        .unwrap_or(file_info.size);

    // 304 Not Modified — the same predicate functions load_file consulted to
    // decide whether the body read could be skipped.
    if let Some(if_none_match) = params.if_none_match.as_deref()
        && let Some((_, etag_value)) = file_info.headers.iter().find(|(k, _)| *k == header::ETAG)
        && if_none_match_hit(if_none_match, etag_value.to_str().unwrap_or_default())
    {
        let mut resp = StatusCode::NOT_MODIFIED.into_response();
        resp.headers_mut().extend(
            file_info
                .headers
                .iter()
                .filter(|(k, _)| *k != header::CONTENT_LENGTH && *k != header::CONTENT_ENCODING)
                .cloned(),
        );
        return Ok(resp);
    }

    // 304 Not Modified (If-Modified-Since)
    if let Some(ims) = params.if_modified_since.as_deref()
        && let Some(secs) = file_info.last_modified_secs
        && if_modified_since_hit(ims, secs)
    {
        let mut resp = StatusCode::NOT_MODIFIED.into_response();
        resp.headers_mut().extend(
            file_info
                .headers
                .iter()
                .filter(|(k, _)| *k != header::CONTENT_LENGTH && *k != header::CONTENT_ENCODING)
                .cloned(),
        );
        return Ok(resp);
    }

    // HEAD: respond with headers only — never read or stream the body.
    if params.head {
        let mut resp = Response::new(Body::empty());
        resp.headers_mut().extend(file_info.headers.iter().cloned());
        return Ok(resp);
    }

    // If-Range: only honor the Range when the cached validator still matches;
    // otherwise fall through to a full 200 so a resumed download of a changed
    // file is not silently corrupted.
    let honor_range = match params.if_range.as_deref() {
        None => true,
        Some(ir) => {
            let etag = file_info
                .headers
                .iter()
                .find(|(k, _)| *k == header::ETAG)
                .and_then(|(_, v)| v.to_str().ok());
            if_range_satisfied(ir, etag, file_info.last_modified_secs)
        }
    };
    let ranges = if honor_range {
        params
            .range
            .as_deref()
            .and_then(|r| parse_ranges(r, total_size))
    } else {
        None
    };

    // A representation that carries its own Content-Encoding (a negotiated
    // pre-compressed sibling, or a backend-set encoding) can't be wrapped in a
    // multipart/byteranges envelope — the envelope itself isn't brotli/gzip and
    // there's no per-representation Content-Encoding for multipart. Fall back to
    // a full 200 for the rare (multi-range + encoded) combination; a single
    // range of an encoded representation is still served as a 206 below.
    let ranges = match ranges {
        Some(RangesValue::Ranges(rs))
            if rs.len() > 1
                && file_info
                    .headers
                    .iter()
                    .any(|(k, _)| *k == header::CONTENT_ENCODING) =>
        {
            None
        }
        other => other,
    };

    // 416 Range Not Satisfiable
    if matches!(ranges, Some(RangesValue::NotSatisfiable)) {
        let mut resp = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        resp.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::try_from(format!("bytes */{total_size}"))
                .unwrap_or_else(|_| HeaderValue::from_static("bytes */*")),
        );
        resp.headers_mut().extend(
            file_info
                .headers
                .iter()
                .filter(|(k, _)| *k != header::CONTENT_LENGTH)
                .cloned(),
        );
        return Ok(resp);
    }

    let single_range = match &ranges {
        Some(RangesValue::Ranges(rs)) if rs.len() == 1 => Some(rs[0]),
        _ => None,
    };
    let is_multipart = matches!(&ranges, Some(RangesValue::Ranges(rs)) if rs.len() > 1);
    let is_partial = single_range.is_some() || is_multipart;

    let mut resp = if let Some((start, end)) = single_range {
        let content_length = end - start + 1;
        let mut resp = if let Some(body) = file_info.body.as_ref() {
            body.slice(start as usize..=end as usize).into_response()
        } else {
            let r = get_storage()?
                .dal
                .reader(&file_info.read_file)
                .await
                .map_err(|e| Error::Openedal { source: e })?;
            let stream = ReaderStream::with_capacity(
                r.into_futures_async_read(start..=end)
                    .await
                    .map_err(|e| Error::Openedal { source: e })?
                    .compat(),
                STREAM_CHUNK_SIZE,
            );
            Body::from_stream(stream).into_response()
        };
        *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
        resp.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::try_from(format!("bytes {start}-{end}/{total_size}"))
                .unwrap_or_else(|_| HeaderValue::from_static("bytes */*")),
        );
        resp.headers_mut().insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&content_length.to_string())
                .unwrap_or_else(|_| HeaderValue::from_static("0")),
        );
        resp
    } else if let Some(RangesValue::Ranges(rs)) = &ranges {
        // Multiple ranges → multipart/byteranges (the single-range case above
        // returns a plain 206). Boundary is unique per response.
        let boundary = next_multipart_boundary(total_size);
        let payload = build_multipart_byteranges(&file_info, rs, total_size, &boundary).await?;
        let content_length = payload.len();
        let mut resp = payload.into_response();
        *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::try_from(format!("multipart/byteranges; boundary={boundary}"))
                .unwrap_or_else(|_| HeaderValue::from_static("multipart/byteranges")),
        );
        resp.headers_mut().insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&content_length.to_string())
                .unwrap_or_else(|_| HeaderValue::from_static("0")),
        );
        resp
    } else if let Some(body) = file_info.body.as_ref() {
        body.clone().into_response()
    } else {
        let r = get_storage()?
            .dal
            .reader(&file_info.read_file)
            .await
            .map_err(|e| Error::Openedal { source: e })?;
        let stream = ReaderStream::with_capacity(
            r.into_futures_async_read(0..)
                .await
                .map_err(|e| Error::Openedal { source: e })?
                .compat(),
            STREAM_CHUNK_SIZE,
        );
        Body::from_stream(stream).into_response()
    };

    // Copy our computed headers onto the response, overwriting rather than
    // appending. A buffered body's `into_response()` (Bytes/Vec) injects a
    // default `Content-Type: application/octet-stream`, and HeaderMap's `extend`
    // *appends* — which would leave that default beside our real value, i.e. two
    // Content-Type headers (the octet-stream one breaks strict ES-module MIME
    // checks, so `.mjs` fails to load). `insert` overwrites; our keys are unique.
    //
    // Partial responses set their own Content-Length above; multipart also
    // replaces Content-Type with the envelope type and drops any representation
    // Content-Encoding — exclude all three from the copy here.
    let resp_headers = resp.headers_mut();
    for (k, v) in file_info.headers.iter() {
        if is_partial && *k == header::CONTENT_LENGTH {
            continue;
        }
        if is_multipart && (*k == header::CONTENT_TYPE || *k == header::CONTENT_ENCODING) {
            continue;
        }
        resp_headers.insert(k, v.clone());
    }

    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_one_range_forms() {
        // explicit, suffix, and open-ended forms against a 1000-byte resource
        assert_eq!(
            parse_one_range("0-99", 1000),
            Some(OneRange::Satisfiable(0, 99))
        );
        assert_eq!(
            parse_one_range("500-", 1000),
            Some(OneRange::Satisfiable(500, 999))
        );
        assert_eq!(
            parse_one_range("-500", 1000),
            Some(OneRange::Satisfiable(500, 999))
        );
        // suffix longer than the resource clamps to the whole body
        assert_eq!(
            parse_one_range("-2000", 1000),
            Some(OneRange::Satisfiable(0, 999))
        );
        // end past EOF clamps to the last byte
        assert_eq!(
            parse_one_range("900-5000", 1000),
            Some(OneRange::Satisfiable(900, 999))
        );
        // whitespace around the spec is tolerated
        assert_eq!(
            parse_one_range(" 0-9 ", 1000),
            Some(OneRange::Satisfiable(0, 9))
        );
    }

    #[test]
    fn parse_one_range_unsatisfiable_and_malformed() {
        // start at/after EOF, or a zero-length suffix, is unsatisfiable
        assert_eq!(
            parse_one_range("1000-", 1000),
            Some(OneRange::Unsatisfiable)
        );
        assert_eq!(parse_one_range("-0", 1000), Some(OneRange::Unsatisfiable));
        // any range against a zero-length resource is unsatisfiable
        assert_eq!(parse_one_range("0-0", 0), Some(OneRange::Unsatisfiable));
        // start > end, missing dash, or non-numeric is malformed (None)
        assert_eq!(parse_one_range("100-50", 1000), None);
        assert_eq!(parse_one_range("abc", 1000), None);
        assert_eq!(parse_one_range("1-x", 1000), None);
    }

    #[test]
    fn parse_ranges_single_and_multi() {
        assert_eq!(
            parse_ranges("bytes=0-99", 1000),
            Some(RangesValue::Ranges(vec![(0, 99)]))
        );
        assert_eq!(
            parse_ranges("bytes=0-99,200-299", 1000),
            Some(RangesValue::Ranges(vec![(0, 99), (200, 299)]))
        );
        // a trailing/empty spec is skipped, not treated as malformed
        assert_eq!(
            parse_ranges("bytes=0-0,", 1000),
            Some(RangesValue::Ranges(vec![(0, 0)]))
        );
    }

    #[test]
    fn parse_ranges_mixed_satisfiability() {
        // an unsatisfiable member of an otherwise-satisfiable set is dropped
        assert_eq!(
            parse_ranges("bytes=0-99,5000-6000", 1000),
            Some(RangesValue::Ranges(vec![(0, 99)]))
        );
        // every member unsatisfiable -> 416
        assert_eq!(
            parse_ranges("bytes=5000-6000", 1000),
            Some(RangesValue::NotSatisfiable)
        );
    }

    #[test]
    fn parse_ranges_rejected_inputs() {
        // wrong unit, or any malformed member, ignores the whole header (full 200)
        assert_eq!(parse_ranges("items=0-99", 1000), None);
        assert_eq!(parse_ranges("bytes=0-99,100-50", 1000), None);
        // more than MAX_RANGES ranges -> ignore the header entirely
        let many = (0..MAX_RANGES + 1)
            .map(|_| "0-0")
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(parse_ranges(&format!("bytes={many}"), 1000), None);
    }

    #[test]
    fn encoding_prefs_negotiation() {
        let prefs = EncodingPrefs::parse("br, gzip");
        assert!(prefs.accepts("br"));
        assert!(prefs.accepts("gzip"));
        assert!(!prefs.accepts("zstd"));

        // q=0 is an explicit refusal, even with a wildcard present
        let prefs = EncodingPrefs::parse("br;q=0, *");
        assert!(!prefs.accepts("br"));
        assert!(prefs.accepts("zstd")); // inherits the wildcard

        // encoding names are case-insensitive
        assert!(EncodingPrefs::parse("BR").accepts("br"));
        // a positive q-value still accepts
        assert!(EncodingPrefs::parse("gzip;q=0.5").accepts("gzip"));
        // a refused wildcard accepts nothing unmentioned
        assert!(!EncodingPrefs::parse("*;q=0").accepts("br"));
    }

    #[test]
    fn if_range_strong_validators_only() {
        // strong ETag match honors the range
        assert!(if_range_satisfied("\"abc\"", Some("\"abc\""), None));
        // mismatch, weak request validator, or weak stored ETag all reject
        assert!(!if_range_satisfied("\"abc\"", Some("\"xyz\""), None));
        assert!(!if_range_satisfied("W/\"abc\"", Some("\"abc\""), None));
        assert!(!if_range_satisfied("\"abc\"", Some("W/\"abc\""), None));

        // date validator: honored only when the resource is not newer than the date
        let date_after = fmt_http_date(UNIX_EPOCH + Duration::from_secs(2000));
        assert!(if_range_satisfied(&date_after, None, Some(1000)));
        let date_before = fmt_http_date(UNIX_EPOCH + Duration::from_secs(500));
        assert!(!if_range_satisfied(&date_before, None, Some(1000)));
    }

    #[test]
    fn encoding_cache_key_is_nul_separated() {
        assert_eq!(encoding_cache_key("a/b.js", "br"), "a/b.js\u{0}br");
        assert_eq!(encoding_cache_key("x", ""), "x\u{0}");
    }

    #[test]
    fn html_escape_covers_dangerous_chars() {
        assert_eq!(
            html_escape("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#x27;"
        );
        // benign text is unchanged
        assert_eq!(html_escape("plain.txt"), "plain.txt");
    }

    #[test]
    fn singleflight_key_varies_by_inputs() {
        let br = Some(EncodingPrefs::parse("br"));
        let gzip = Some(EncodingPrefs::parse("gzip"));
        let none: Option<EncodingPrefs> = None;

        // identical inputs collapse to the same key (so they single-flight)
        assert_eq!(
            singleflight_key("a.js", &br, false),
            singleflight_key("a.js", &br, false)
        );
        // a different accepted-encoding set must not share a leader
        assert_ne!(
            singleflight_key("a.js", &br, false),
            singleflight_key("a.js", &gzip, false)
        );
        // HEAD (no body) is a distinct response from GET
        assert_ne!(
            singleflight_key("a.js", &br, true),
            singleflight_key("a.js", &br, false)
        );
        // a different path is a different key
        assert_ne!(
            singleflight_key("a.js", &br, false),
            singleflight_key("b.js", &br, false)
        );
        // no Accept-Encoding -> all four flag bits are 0
        assert!(singleflight_key("a.js", &none, false).ends_with("\u{0}0000"));
    }

    #[test]
    fn next_multipart_boundary_is_unique_and_formatted() {
        let b1 = next_multipart_boundary(0xABCD);
        let b2 = next_multipart_boundary(0xABCD);
        // the size is hex-encoded into the token
        assert!(b1.starts_with("static_serve_abcd_"));
        assert!(b2.starts_with("static_serve_abcd_"));
        // the monotonic counter makes successive boundaries distinct
        assert_ne!(b1, b2);
    }

    #[test]
    fn build_multipart_byteranges_buffered() {
        // A buffered (in-memory) body assembles the multipart envelope without
        // touching storage. Two ranges over a 10-byte body, fixed boundary.
        let info = FileInfo {
            headers: vec![(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"))],
            body: Some(Bytes::from_static(b"0123456789")),
            size: 10,
            read_file: String::new(),
            last_modified_secs: None,
        };
        let ranges = [(0u64, 2u64), (5u64, 7u64)];
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let out = rt
            .block_on(build_multipart_byteranges(&info, &ranges, 10, "BOUND"))
            .expect("multipart assembly");
        let expected = "--BOUND\r\nContent-Type: text/plain\r\nContent-Range: bytes 0-2/10\r\n\r\n012\r\n\
                        --BOUND\r\nContent-Type: text/plain\r\nContent-Range: bytes 5-7/10\r\n\r\n567\r\n\
                        --BOUND--\r\n";
        assert_eq!(out.as_ref(), expected.as_bytes());
    }

    #[test]
    fn encoding_prefs_edge_cases() {
        // an empty header accepts nothing
        let empty = EncodingPrefs::parse("");
        assert!(!empty.accepts("br"));
        assert!(!empty.accepts("gzip"));
        // whitespace around the name and the q-param is tolerated
        assert!(EncodingPrefs::parse(" gzip ; q=0.5 ").accepts("gzip"));
        // a tiny positive q still accepts
        assert!(EncodingPrefs::parse("br;q=0.001").accepts("br"));
        // unrecognised tokens are ignored, not fatal
        let mixed = EncodingPrefs::parse("identity, deflate, br");
        assert!(mixed.accepts("br"));
        assert!(!mixed.accepts("gzip")); // no wildcard, so gzip is unaccepted
        // first occurrence wins: an initial q=0 refusal is not overridden
        assert!(!EncodingPrefs::parse("br;q=0, br;q=1").accepts("br"));
    }

    #[test]
    fn join_index_inserts_single_slash() {
        // a trailing slash is not doubled
        assert_eq!(join_index("foo/", "index.html"), "foo/index.html");
        // a missing slash is inserted
        assert_eq!(join_index("foo", "index.html"), "foo/index.html");
        // nested dirs keep their interior slashes
        assert_eq!(join_index("a/b/", "index.html"), "a/b/index.html");
        // the root (empty dir) keeps parity with the historical join: the
        // optimistic probe and the fallback must target the same key.
        assert_eq!(join_index("", "index.html"), "/index.html");
    }

    #[test]
    fn cache_negative_and_positive_roundtrip() {
        let size = 16;
        // a stored negative entry comes back as NotFound...
        set_file_to_cache(
            "missing.js\u{0}".to_string(),
            CacheValue::NotFound,
            size,
            Duration::from_secs(60),
        );
        let now = now_unix_secs();
        assert!(matches!(
            get_file_from_cache(&"missing.js\u{0}".to_string(), size, now),
            Some(CacheValue::NotFound)
        ));
        // ...and a negative hit replays a not-found error
        let res = cached_result(CacheValue::NotFound, "missing.js");
        assert!(matches!(res, Err(e) if e.is_not_found()));

        // a positive entry round-trips the shared FileInfo
        let info = Arc::new(FileInfo {
            headers: vec![],
            body: Some(Bytes::from_static(b"x")),
            size: 1,
            read_file: "a.js".to_string(),
            last_modified_secs: None,
        });
        set_file_to_cache(
            "a.js\u{0}".to_string(),
            CacheValue::Found(info),
            size,
            Duration::from_secs(60),
        );
        match get_file_from_cache(&"a.js\u{0}".to_string(), size, now) {
            Some(CacheValue::Found(got)) => assert_eq!(got.size, 1),
            _ => panic!("expected a positive cache hit"),
        }

        // a zero TTL means the entry is already expired on the next lookup
        set_file_to_cache(
            "expired\u{0}".to_string(),
            CacheValue::NotFound,
            size,
            Duration::ZERO,
        );
        assert!(get_file_from_cache(&"expired\u{0}".to_string(), size, now_unix_secs()).is_none());
    }

    #[test]
    fn parse_ranges_caps_total_multipart_bytes() {
        let total = 100u64 * 1024 * 1024 * 1024; // 100 GiB representation
        let five_mb = 5u64 * 1024 * 1024;
        // two 5 MiB ranges sum past the 8 MiB cap -> ignore the header (full 200)
        let header = format!("bytes=0-{},{}-{}", five_mb - 1, five_mb, 2 * five_mb - 1);
        assert_eq!(parse_ranges(&header, total), None);
        // a single range of any size is exempt — it streams as a plain 206
        assert_eq!(
            parse_ranges(&format!("bytes=0-{}", 2 * five_mb - 1), total),
            Some(RangesValue::Ranges(vec![(0, 2 * five_mb - 1)]))
        );
        // multiple small ranges stay under the cap and are served
        assert_eq!(
            parse_ranges("bytes=0-9,20-29", total),
            Some(RangesValue::Ranges(vec![(0, 9), (20, 29)]))
        );
        // overlapping ranges are summed as requested, not deduped: 3x a 3 MiB
        // slice is 9 MiB of buffered payload and is rejected
        let three_mb = 3u64 * 1024 * 1024;
        let overlap = format!("bytes=0-{0},0-{0},0-{0}", three_mb - 1);
        assert_eq!(parse_ranges(&overlap, total), None);
    }

    #[test]
    fn flight_guard_removes_slot_and_wakes_followers_on_drop() {
        let key = "guard-test.js\u{0}0000".to_string();
        let flight = Arc::new(Flight::new());
        inflight()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), flight.clone());

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        rt.block_on(async {
            // register a follower first, exactly like get_file does
            let notified = flight.done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            // dropping the guard (leader cancelled) must remove the slot and
            // wake the follower — otherwise this await would hang forever
            drop(FlightGuard { key: key.clone() });
            notified.await;
        });
        assert!(
            !inflight()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&key)
        );
        // no result was published, so the follower falls back to its own load
        assert!(flight.result().is_none());
    }

    #[test]
    fn compressible_content_types() {
        assert!(is_compressible_content_type("text/html; charset=utf-8"));
        assert!(is_compressible_content_type("application/javascript"));
        assert!(is_compressible_content_type("application/wasm"));
        assert!(is_compressible_content_type("image/svg+xml"));
        // already-compressed formats stay excluded
        assert!(!is_compressible_content_type("image/png"));
        assert!(!is_compressible_content_type("video/mp4"));
        assert!(!is_compressible_content_type("application/octet-stream"));
    }

    #[test]
    fn compress_body_shrinks_repetitive_input() {
        // highly repetitive input must compress under every supported encoding;
        // an unknown encoding yields None (callers keep the identity body)
        let input = vec![b'a'; 64 * 1024];
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        rt.block_on(async {
            for enc in ["br", "zstd", "gzip"] {
                let out = compress_body(enc, CompressionLevel::Default, &input)
                    .await
                    .expect("supported encoding compresses");
                assert!(
                    out.len() < input.len(),
                    "{enc} output should shrink: {} vs {}",
                    out.len(),
                    input.len()
                );
            }
            assert!(
                compress_body("deflate", CompressionLevel::Default, &input)
                    .await
                    .is_none()
            );
        });
    }

    #[test]
    fn conditional_predicates() {
        // If-None-Match: wildcard, exact, comma list with spaces, mismatch
        assert!(if_none_match_hit("*", "\"abc\""));
        assert!(if_none_match_hit("\"abc\"", "\"abc\""));
        assert!(if_none_match_hit("\"x\", \"abc\"", "\"abc\""));
        assert!(!if_none_match_hit("\"x\"", "\"abc\""));

        // If-Modified-Since: not newer -> hit, newer -> miss, garbage -> miss
        let at_2000 = fmt_http_date(UNIX_EPOCH + Duration::from_secs(2000));
        assert!(if_modified_since_hit(&at_2000, 1000));
        assert!(if_modified_since_hit(&at_2000, 2000));
        assert!(!if_modified_since_hit(&at_2000, 3000));
        assert!(!if_modified_since_hit("not a date", 1000));
    }

    #[test]
    fn conditional_not_modified_combines_both_validators() {
        let mut params = StaticServeParams {
            if_none_match: Some(Arc::from("\"abc\"")),
            ..Default::default()
        };
        // ETag arm hits independently of Last-Modified
        assert!(conditional_not_modified(&params, Some("\"abc\""), None));
        assert!(!conditional_not_modified(&params, Some("\"other\""), None));
        // no stored ETag -> the INM arm can't hit
        assert!(!conditional_not_modified(&params, None, Some(1000)));

        // IMS arm hits when INM missed (mirrors static_serve's two checks)
        params.if_modified_since = Some(Arc::from(
            fmt_http_date(UNIX_EPOCH + Duration::from_secs(2000)).as_str(),
        ));
        assert!(conditional_not_modified(
            &params,
            Some("\"other\""),
            Some(1000)
        ));

        // no conditional headers at all -> never a hit
        let bare = StaticServeParams::default();
        assert!(!conditional_not_modified(&bare, Some("\"abc\""), Some(0)));
    }

    #[test]
    fn not_found_page_cache_roundtrip() {
        let now = now_unix_secs();
        // nothing stored yet -> miss (no other test touches this slot)
        assert!(cached_not_found_page(now).is_none());
        // a stored body comes back until the TTL passes
        let body = Bytes::from_static(b"<h1>404</h1>");
        store_not_found_page(Some(body.clone()), Duration::from_secs(60));
        assert_eq!(cached_not_found_page(now), Some(Some(body)));
        // the page's absence is a cacheable result too
        store_not_found_page(None, Duration::from_secs(60));
        assert_eq!(cached_not_found_page(now), Some(None));
        // a zero TTL is already expired on the next lookup
        store_not_found_page(None, Duration::ZERO);
        assert!(cached_not_found_page(now_unix_secs()).is_none());
    }

    #[test]
    fn parse_ranges_more_edges() {
        // "bytes=" with no actual spec -> ignore the header (full 200)
        assert_eq!(parse_ranges("bytes=", 1000), None);
        // whitespace within the list is tolerated
        assert_eq!(
            parse_ranges("bytes= 0-9 , 20-29 ", 1000),
            Some(RangesValue::Ranges(vec![(0, 9), (20, 29)]))
        );
        // a suffix length >= the body clamps to the whole representation
        assert_eq!(
            parse_ranges("bytes=-1000", 1000),
            Some(RangesValue::Ranges(vec![(0, 999)]))
        );
    }
}
