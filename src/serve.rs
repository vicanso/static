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
use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bstr::ByteSlice;
use bytes::Bytes;
use bytesize::ByteSize;
use httpdate::{fmt_http_date, parse_http_date};
use std::collections::HashMap;
use std::fmt::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tinyufo::TinyUfo;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tokio_util::io::ReaderStream;

pub static X_ORIGINAL_SIZE_HEADER_NAME: HeaderName = HeaderName::from_static("x-original-size");

// Static HTML template for directory listing view
// Includes basic styling and JavaScript for date formatting
static WEB_HTML: &str = r###"<!doctype html>
<html lang="en">
    <head>
        <meta charset="utf-8" />
        <style>
            * {
                margin: 0;
                padding: 0;
            }
            table {
                width: 100%;
            }
            a {
                color: #333;
            }
            .size {
                width: 180px;
                text-align: left;
            }
            .lastModified {
                width: 280px;
                text-align: right;
            }
            th, td {
                padding: 10px;
            }
            thead {
                background-color: #f0f0f0;
            }
            tr:nth-child(even) {
                background-color: #f0f0f0;
            }
        </style>
        <script type="text/javascript">
        function updateAllLastModified() {
            Array.from(document.getElementsByClassName("lastModified")).forEach((item, index) => {
                if (index == 0) {
                    return;
                }
                const ts = item.innerHTML;
                if (!ts) {
                    item.innerHTML = "--";
                    return;
                }
                const date = new Date(ts * 1000);
                if (isFinite(date)) {
                    item.innerHTML = date.toLocaleString();
                }
            });
        }
        document.addEventListener("DOMContentLoaded", (event) => {
          updateAllLastModified();
        });
        </script>
    </head>
    <body>
        <table border="0" cellpadding="0" cellspacing="0">
            <thead>
                <th class="name">File Name</th>
                <th class="size">Size</th>
                <th class="lastModified">Last Modified</th>
            </thread>
            <tbody>
                {{CONTENT}}
            </tobdy>
        </table>
    </body>
</html>
"###;

async fn get_autoindex_html(path: &str) -> Result<String> {
    let entry_list = get_storage()?
        .dal
        .list(path)
        .await
        .map_err(|e| Error::Openedal { source: e })?;
    let mut html_rows = String::with_capacity(entry_list.len() * 128);
    for entry in entry_list {
        let filepath = entry.path();
        let name = entry.name();
        if name.len() <= 1 || name.starts_with('.') {
            continue;
        }

        let meta = entry.metadata();
        let mut size = "".to_string();
        let mut last_modified = "".to_string();
        if meta.is_file() {
            size = ByteSize(meta.content_length()).to_string();
            if let Some(value) = meta.last_modified() {
                last_modified = value.to_string();
            }
        }

        let _ = write!(
            html_rows,
            r###"<tr>
                <td class="name"><a href="./{filepath}">{name}</a></td>
                <td class="size">{size}</td>
                <td class="lastModified">{last_modified}</td>
            </tr>"###
        );
    }

    Ok(WEB_HTML.replace("{{CONTENT}}", &html_rows))
}

#[derive(Debug, Clone, Default)]
pub struct StaticServeParams {
    pub file: String,
    pub index: String,
    pub autoindex: bool,
    pub cache_control: String,
    pub cache_control_map: Arc<HashMap<String, String>>,
    pub html_replaces: Arc<Vec<(Vec<u8>, Vec<u8>)>>,
    pub cache_size: usize,
    pub cache_ttl: Duration,
    pub range: Option<String>,
    pub if_none_match: Option<String>,
    pub if_modified_since: Option<String>,
    pub accept_encoding: Option<String>,
    pub read_max_size: u64,
}

#[derive(Clone)]
struct FileInfoCache {
    expired_at: u64,
    data: FileInfo,
}

#[derive(Clone)]
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

fn get_file_from_cache(file: &String, cache_size: usize) -> Option<FileInfo> {
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

fn set_file_to_cache(file: &str, info: &FileInfo, cache_size: usize, cache_ttl: Duration) {
    let expired_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + cache_ttl.as_secs();
    get_static_cache(cache_size).put(
        file.to_string(),
        FileInfoCache {
            expired_at,
            data: info.clone(),
        },
        1,
    );
}

async fn get_file(params: &StaticServeParams) -> Result<FileInfo> {
    let mut file = params.file.clone();
    if params.cache_size > 0
        && let Some(info) = get_file_from_cache(&file, params.cache_size)
    {
        return Ok(info);
    }
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
    let mut headers = Vec::with_capacity(8);
    headers.push((header::ACCEPT_RANGES, HeaderValue::from_static("bytes")));

    if is_dir && params.autoindex {
        let html = get_autoindex_html(&file).await?;
        headers.push((header::CONTENT_TYPE, HeaderValue::from_static("text/html")));
        headers.push((header::CACHE_CONTROL, HeaderValue::from_static("no-cache")));
        let body = Bytes::from(html);
        let info = FileInfo {
            size: body.len() as u64,
            headers,
            body: Some(body),
            read_file: file.clone(),
            last_modified_secs: None,
        };
        if params.cache_size > 0 {
            set_file_to_cache(&file, &info, params.cache_size, params.cache_ttl);
        }
        return Ok(info);
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
            params.cache_control.clone()
        }
    };
    if let Ok(v) = HeaderValue::try_from(cache_control) {
        headers.push((header::CACHE_CONTROL, v));
    }
    if let Ok(v) = HeaderValue::try_from(content_type) {
        headers.push((header::CONTENT_TYPE, v));
    }
    // Try pre-compressed file (.br / .gz) if enabled and client supports it
    let mut precompressed_file = None;
    if let Some(ref accept_encoding) = params.accept_encoding
        && !is_html
        && !is_dir
    {
        // Priority: brotli > gzip
        let candidates: &[(&str, &str)] = &[("br", ".br"), ("gzip", ".gz")];
        for (encoding, ext) in candidates {
            if accept_encoding.contains(encoding) {
                let compressed = format!("{file}{ext}");
                if let Ok(compressed_meta) = storage.dal.stat(&compressed).await {
                    precompressed_file = Some(compressed);
                    meta = compressed_meta;
                    headers.push((header::CONTENT_ENCODING, HeaderValue::from_static(encoding)));
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
    // Add Vary: Accept-Encoding when precompressed feature is enabled so CDN
    // caches compressed and uncompressed variants separately
    if params.accept_encoding.is_some() && !is_html && !is_dir {
        headers.push((header::VARY, HeaderValue::from_static("Accept-Encoding")));
    }

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
    let body = if is_html || size < params.read_max_size {
        let mut buf = storage
            .dal
            .read(read_file)
            .await
            .map_err(|e| Error::Openedal { source: e })?
            .to_vec();

        // Only apply HTML replacements to HTML content
        if is_html {
            for (key, value) in params.html_replaces.iter() {
                buf = buf.replace(key, value);
            }
        }
        Some(Bytes::from(buf))
    } else {
        None
    };
    let read_path = precompressed_file.unwrap_or_else(|| file.clone());
    let info = FileInfo {
        headers,
        body,
        size,
        read_file: read_path,
        last_modified_secs,
    };
    if params.cache_size > 0 && !is_html && info.body.is_some() {
        set_file_to_cache(&file, &info, params.cache_size, params.cache_ttl);
    }

    Ok(info)
}

#[derive(Clone, Copy)]
enum RangeValue {
    Satisfiable(u64, u64),
    NotSatisfiable,
}

fn parse_range(range_header: &str, total_size: u64) -> Option<RangeValue> {
    let range_str = range_header.strip_prefix("bytes=")?;
    if range_str.contains(',') {
        return None; // multi-range not supported
    }
    let (start_str, end_str) = range_str.split_once('-')?;

    if total_size == 0 {
        return Some(RangeValue::NotSatisfiable);
    }

    let (start, end) = if start_str.is_empty() {
        // bytes=-500
        let suffix_len: u64 = end_str.parse().ok()?;
        if suffix_len == 0 {
            return Some(RangeValue::NotSatisfiable);
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
            return Some(RangeValue::NotSatisfiable);
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
            return Some(RangeValue::NotSatisfiable);
        }
        (start, end.min(total_size - 1))
    };

    Some(RangeValue::Satisfiable(start, end))
}

// 处理函数
pub async fn static_serve(params: StaticServeParams) -> Result<Response> {
    let mut file_info = get_file(&params).await?;
    let read_file = std::mem::take(&mut file_info.read_file);
    let total_size = file_info
        .body
        .as_ref()
        .map(|b| b.len() as u64)
        .unwrap_or(file_info.size);

    // 304 Not Modified
    if let Some(ref if_none_match) = params.if_none_match
        && let Some((_, etag_value)) = file_info.headers.iter().find(|(k, _)| *k == header::ETAG)
    {
        let etag_str = etag_value.to_str().unwrap_or_default();
        if if_none_match == "*" || if_none_match.split(',').any(|v| v.trim() == etag_str) {
            let mut resp = StatusCode::NOT_MODIFIED.into_response();
            resp.headers_mut().extend(
                file_info.headers.into_iter().filter(|(k, _)| {
                    *k != header::CONTENT_LENGTH && *k != header::CONTENT_ENCODING
                }),
            );
            return Ok(resp);
        }
    }

    // 304 Not Modified (If-Modified-Since)
    if let Some(ref ims) = params.if_modified_since
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
                .into_iter()
                .filter(|(k, _)| *k != header::CONTENT_LENGTH && *k != header::CONTENT_ENCODING),
        );
        return Ok(resp);
    }

    let range = params
        .range
        .as_deref()
        .and_then(|r| parse_range(r, total_size));

    // 416 Range Not Satisfiable
    if matches!(range, Some(RangeValue::NotSatisfiable)) {
        let mut resp = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        resp.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::try_from(format!("bytes */{total_size}"))
                .unwrap_or_else(|_| HeaderValue::from_static("bytes */*")),
        );
        resp.headers_mut().extend(
            file_info
                .headers
                .into_iter()
                .filter(|(k, _)| *k != header::CONTENT_LENGTH),
        );
        return Ok(resp);
    }

    let is_partial = matches!(range, Some(RangeValue::Satisfiable(_, _)));

    let mut resp = if let Some(RangeValue::Satisfiable(start, end)) = range {
        let content_length = end - start + 1;
        let mut resp = if let Some(body) = file_info.body.take() {
            body.slice(start as usize..=end as usize).into_response()
        } else {
            let r = get_storage()?
                .dal
                .reader(&read_file)
                .await
                .map_err(|e| Error::Openedal { source: e })?;
            let stream = ReaderStream::new(
                r.into_futures_async_read(start..=end)
                    .await
                    .map_err(|e| Error::Openedal { source: e })?
                    .compat(),
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
    } else {
        if let Some(body) = file_info.body.take() {
            body.into_response()
        } else {
            let r = get_storage()?
                .dal
                .reader(&read_file)
                .await
                .map_err(|e| Error::Openedal { source: e })?;
            let stream = ReaderStream::new(
                r.into_futures_async_read(0..)
                    .await
                    .map_err(|e| Error::Openedal { source: e })?
                    .compat(),
            );
            Body::from_stream(stream).into_response()
        }
    };

    resp.headers_mut().extend(
        file_info
            .headers
            .into_iter()
            .filter(|(k, _)| !(is_partial && *k == header::CONTENT_LENGTH)),
    );

    Ok(resp)
}
