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
use crate::storage::get_storage;
use aho_corasick::AhoCorasick;
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
use tokio::sync::Notify;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tokio_util::io::ReaderStream;

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

async fn get_autoindex_html(path: &str) -> Result<String> {
    let entry_list = get_storage()?
        .dal
        .list(path)
        .await
        .map_err(|e| Error::Openedal { source: e })?;
    let mut html_rows = String::with_capacity(entry_list.len() * 128);
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

    Ok(WEB_HTML.replace("{{CONTENT}}", &html_rows))
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
    // Per-request header echoes — Arc<str> so the fallback retry loop in
    // main.rs can clone/share them at refcount cost instead of allocating
    // fresh String copies per iteration.
    pub range: Option<Arc<str>>,
    pub if_range: Option<Arc<str>>,
    pub if_none_match: Option<Arc<str>>,
    pub if_modified_since: Option<Arc<str>>,
    pub accept_encoding: Option<Arc<str>>,
    pub read_max_size: u64,
    pub head: bool,
    pub request_path: Arc<str>,
    pub request_query: Option<Arc<str>>,
}

#[derive(Clone)]
struct FileInfoCache {
    expired_at: u64,
    // Arc-shared so cache hits cost a refcount bump, not a deep clone of
    // `headers` (Vec) + `read_file` (String). Bytes is already cheap-clone.
    data: Arc<FileInfo>,
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

fn get_file_from_cache(file: &String, cache_size: usize) -> Option<Arc<FileInfo>> {
    if let Some(info) = get_static_cache(cache_size).get(file)
        && info.expired_at
            > SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
    {
        return Some(info.data.clone());
    }
    None
}

fn set_file_to_cache(file: &str, info: Arc<FileInfo>, cache_size: usize, cache_ttl: Duration) {
    let expired_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + cache_ttl.as_secs();
    get_static_cache(cache_size).put(
        file.to_string(),
        FileInfoCache {
            expired_at,
            data: info,
        },
        1,
    );
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
        if let Some(prefs) = &accept_prefs {
            for enc in ["br", "zstd", "gzip"] {
                if prefs.accepts(enc)
                    && let Some(info) =
                        get_file_from_cache(&encoding_cache_key(&file, enc), params.cache_size)
                {
                    crate::metrics::record_cache_hit();
                    return Ok(info);
                }
            }
        }
        if let Some(info) = get_file_from_cache(&encoding_cache_key(&file, ""), params.cache_size) {
            crate::metrics::record_cache_hit();
            return Ok(info);
        }
        crate::metrics::record_cache_miss();
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
        return load_file(params, file, &accept_prefs).await;
    }

    // Leader: load, hand the result to any followers, then clear the slot.
    let res = load_file(params, file, &accept_prefs).await;
    if let Some(flight) = inflight()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&sf_key)
    {
        if let Ok(info) = &res {
            flight.publish(info.clone());
        }
        flight.done.notify_waiters();
    }
    res
}

async fn load_file(
    params: &StaticServeParams,
    mut file: String,
    accept_prefs: &Option<EncodingPrefs>,
) -> Result<Arc<FileInfo>> {
    let storage = get_storage()?;
    storage.validate(&file)?;

    let mut meta = storage
        .dal
        .stat(&file)
        .await
        .map_err(|e| Error::Openedal { source: e })?;

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
    let mut headers = Vec::with_capacity(8);
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
        return Ok(Arc::new(info));
    }
    if is_dir && !params.index.is_empty() {
        file = if file.ends_with("/") {
            format!("{file}{}", params.index)
        } else {
            format!("{file}/{}", params.index)
        };
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
    if let Ok(v) = HeaderValue::try_from(content_type) {
        headers.push((header::CONTENT_TYPE, v));
    }
    // Try pre-compressed file (.br / .zst / .gz) if enabled and client supports
    // it. `negotiated_encoding` is the encoding actually served ("" = identity)
    // and becomes the cache-key suffix below. Priority: brotli > zstd > gzip.
    let mut precompressed_file = None;
    let mut negotiated_encoding: &'static str = "";
    if let Some(prefs) = accept_prefs
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
    if let Some(etag) = etag
        && let Ok(v) = HeaderValue::try_from(etag)
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

    // size.to_string() is decimal digits — always a valid HeaderValue
    if let Ok(v) = HeaderValue::from_str(&size.to_string()) {
        headers.push((X_ORIGINAL_SIZE_HEADER_NAME.clone(), v.clone()));
        headers.push((header::CONTENT_LENGTH, v));
    }

    // read html or small file
    let read_file = precompressed_file.as_deref().unwrap_or(&file);
    let body = if !params.head && (is_html || size < params.read_max_size) {
        let mut buf = storage
            .dal
            .read(read_file)
            .await
            .map_err(|e| Error::Openedal { source: e })?
            .to_vec();

        // Only apply HTML replacements to HTML content. Single pass: the
        // Aho-Corasick automaton walks the body once and emits one new Vec,
        // regardless of how many (key, value) pairs are configured.
        if is_html && let Some(replacer) = &params.html_replacer {
            buf = replacer.replace_all_bytes(&buf);
        }
        Some(Bytes::from(buf))
    } else {
        None
    };
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
    if params.cache_size > 0 && !is_html && !params.head {
        let key = encoding_cache_key(&file, negotiated_encoding);
        set_file_to_cache(&key, info.clone(), params.cache_size, params.cache_ttl);
    }

    Ok(info)
}

// A parsed `Range` header. `Ranges` holds one or more satisfiable byte ranges
// (inclusive, clamped to the representation) in request order: a single entry
// yields a `206` with `Content-Range`, multiple entries a `multipart/byteranges`
// body.
#[derive(Clone)]
enum RangesValue {
    NotSatisfiable,
    Ranges(Vec<(u64, u64)>),
}

enum OneRange {
    Satisfiable(u64, u64),
    Unsatisfiable,
}

// Cap on ranges accepted in a single request. A client can otherwise amplify one
// request into thousands of tiny reads / multipart parts; beyond this we ignore
// the Range header entirely and serve the full 200.
const MAX_RANGES: usize = 100;

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
// is used almost exclusively for small seeks (media, PDFs) and the range count is
// capped by MAX_RANGES.
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

    let mut buf: Vec<u8> = Vec::new();
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
            buf.extend_from_slice(&chunk.to_vec());
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

    // 304 Not Modified
    if let Some(if_none_match) = params.if_none_match.as_deref()
        && let Some((_, etag_value)) = file_info.headers.iter().find(|(k, _)| *k == header::ETAG)
    {
        let etag_str = etag_value.to_str().unwrap_or_default();
        if if_none_match == "*" || if_none_match.split(',').any(|v| v.trim() == etag_str) {
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
    }

    // 304 Not Modified (If-Modified-Since)
    if let Some(ims) = params.if_modified_since.as_deref()
        && let Some(secs) = file_info.last_modified_secs
        && let Ok(ims_time) = parse_http_date(ims)
        && let Ok(ims_secs) = ims_time
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
        && secs <= ims_secs
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

    // Partial responses set their own Content-Length above; multipart also
    // replaces Content-Type with the envelope type and drops any representation
    // Content-Encoding — exclude all three from the header copy here.
    resp.headers_mut().extend(
        file_info
            .headers
            .iter()
            .filter(|(k, _)| {
                if is_partial && *k == header::CONTENT_LENGTH {
                    return false;
                }
                if is_multipart && (*k == header::CONTENT_TYPE || *k == header::CONTENT_ENCODING) {
                    return false;
                }
                true
            })
            .cloned(),
    );

    Ok(resp)
}
