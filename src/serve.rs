// Copyright 2025 Tree xie.
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
use axum::http::{header, HeaderName, HeaderValue};
use axum::response::{IntoResponse, Response};
use bstr::ByteSlice;
use bytesize::ByteSize;
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tinyufo::TinyUfo;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tokio_util::io::ReaderStream;

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
                const ts = item.innerHTM;
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
    let mut file_list_html = vec![];
    let entry_list = get_storage()?
        .dal
        .list(path)
        .await
        .map_err(|e| Error::Openedal { source: e })?;
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
                last_modified = value.timestamp().to_string();
            }
        }

        let target = format!("./{filepath}");

        file_list_html.push(format!(
            r###"<tr>
                <td class="name"><a href="{target}">{name}</a></td>
                <td class="size">{size}</td>
                <td class="lastModified">{last_modified}</td>
            </tr>"###
        ));
    }

    Ok(WEB_HTML.replace("{{CONTENT}}", &file_list_html.join("\n")))
}

#[derive(Debug, Clone, Default)]
pub struct StaticServeParams {
    pub file: String,
    pub index: String,
    pub autoindex: bool,
    pub cache_control: String,
    pub html_replaces: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Clone)]
struct FileInfoCache {
    expired_at: u64,
    data: FileInfo,
}

#[derive(Clone)]
struct FileInfo {
    headers: Vec<(HeaderName, String)>,
    body: Option<Vec<u8>>,
}

static STATIC_CACHE_TTL: LazyLock<Duration> = LazyLock::new(|| {
    let value = std::env::var("STATIC_CACHE_TTL").unwrap_or("10m".to_string());
    humantime::parse_duration(&value).unwrap_or(Duration::from_secs(10 * 60))
});

static STATIC_CACHE_SIZE: LazyLock<usize> = LazyLock::new(|| {
    let value = std::env::var("STATIC_CACHE_SIZE").unwrap_or("1024".to_string());
    value.parse::<usize>().unwrap_or(1024)
});

static STATIC_CACHE: LazyLock<TinyUfo<String, FileInfoCache>> =
    LazyLock::new(|| TinyUfo::new(*STATIC_CACHE_SIZE, *STATIC_CACHE_SIZE));

fn get_file_from_cache(file: &String) -> Option<FileInfo> {
    if *STATIC_CACHE_SIZE == 0 {
        return None;
    }
    if let Some(info) = STATIC_CACHE.get(file) {
        if info.expired_at
            > SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        {
            return Some(info.data.clone());
        }
    }
    None
}

fn set_file_to_cache(file: &str, info: &FileInfo) {
    if *STATIC_CACHE_SIZE == 0 {
        return;
    }
    let expired_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + STATIC_CACHE_TTL.as_secs();
    STATIC_CACHE.put(
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
    if let Some(info) = get_file_from_cache(&file) {
        return Ok(info);
    }

    let mut meta = get_storage()?
        .dal
        .stat(&file)
        .await
        .map_err(|e| Error::Openedal { source: e })?;

    let is_dir = meta.is_dir();
    if is_dir && !params.autoindex && params.index.is_empty() {
        return Err(Error::NotFound { file: file.clone() });
    }
    let mut headers = Vec::with_capacity(4);

    if is_dir && params.autoindex {
        let html = get_autoindex_html(&file).await?;
        headers.push((header::CONTENT_TYPE, "text/html".to_string()));
        headers.push((header::CACHE_CONTROL, "no-cache".to_string()));
        return Ok(FileInfo {
            headers,
            body: Some(html.into_bytes()),
        });
    }
    if is_dir && !params.index.is_empty() {
        file = if file.ends_with("/") {
            format!("{file}{}", params.index)
        } else {
            format!("{file}/{}", params.index)
        };
        meta = get_storage()?
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
    if content_type.contains("text/html") {
        is_html = true;
        headers.push((header::CACHE_CONTROL, "no-cache".to_string()));
    } else if let Some(cache_control) = meta.cache_control() {
        headers.push((header::CACHE_CONTROL, cache_control.to_string()));
    } else {
        headers.push((header::CACHE_CONTROL, params.cache_control.clone()));
    }
    headers.push((header::CONTENT_TYPE, content_type));
    if let Some(content_encoding) = meta.content_encoding() {
        headers.push((header::CONTENT_ENCODING, content_encoding.to_string()));
    }

    let size = meta.content_length();
    // Generate ETag based on file size and modification time
    if let Some(etag) = meta.etag() {
        headers.push((header::ETAG, etag.to_string()));
    } else if let Some(last_modified) = meta.last_modified() {
        let value = last_modified.timestamp();
        if value > 0 {
            let etag = format!(r#"W/"{:x}-{:x}""#, size, value);
            headers.push((header::ETAG, etag));
        }
    }

    // read html or small file
    let body = if is_html || size < 30 * 1024 {
        let mut buf = get_storage()?
            .dal
            .read(&file)
            .await
            .map_err(|e| Error::Openedal { source: e })?
            .to_vec();

        for (key, value) in params.html_replaces.iter() {
            buf = buf.replace(key, value);
        }
        Some(buf)
    } else {
        None
    };
    let info = FileInfo { headers, body };
    if !is_html && info.body.is_some() {
        set_file_to_cache(&file, &info);
    }

    Ok(info)
}

// 处理函数
pub async fn static_serve(params: StaticServeParams) -> Result<Response> {
    let file_info = get_file(&params).await?;

    let mut resp = if let Some(body) = file_info.body {
        body.into_response()
    } else {
        let r = get_storage()?.dal.reader(&params.file).await.unwrap();
        let stream = ReaderStream::new(r.into_futures_async_read(0..).await.unwrap().compat());
        let body = Body::from_stream(stream);
        body.into_response()
    };

    file_info.headers.iter().for_each(|(k, v)| {
        let Ok(value) = HeaderValue::from_str(v) else {
            return;
        };
        resp.headers_mut().insert(k, value);
    });

    Ok(resp)
}
