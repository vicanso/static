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
use axum::body::Body;
use axum::http::{header, HeaderName, HeaderValue};
use axum::response::{IntoResponse, Response};
use bytesize::ByteSize;
use glob::glob;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tinyufo::TinyUfo;
use tokio::fs;
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
            Array.from(document.getElementsByClassName("lastModified")).forEach((item) => {
                const date = new Date(item.innerHTML * 1000);
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

fn get_autoindex_html(path: &Path) -> Result<String> {
    let path = path.to_string_lossy();
    let mut file_list_html = vec![];
    for entry in glob(&format!("{path}/*")).map_err(|e| Error::Pattern { source: e })? {
        let f = entry.map_err(|e| Error::Glob { source: e })?;
        let filepath = f.to_string_lossy();
        let mut size = "".to_string();
        let mut last_modified = "".to_string();
        let mut is_file = false;
        if f.is_file() {
            is_file = true;
            #[cfg(unix)]
            let _ = f.metadata().map(|meta| {
                size = ByteSize(meta.size()).to_string();
                last_modified = meta.mtime().to_string();
            });
        }

        let name = f.file_name().unwrap_or_default().to_string_lossy();
        if name.is_empty() || name.starts_with('.') {
            continue;
        }

        let mut target = format!("./{}", filepath.split('/').last().unwrap_or_default());
        if !is_file {
            target += "/";
        }
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
    pub dir: String,
    pub file: String,
    pub index: String,
    pub autoindex: bool,
    pub cache_control: String,
}

#[derive(Clone)]
struct FileInfoCache {
    expired_at: u64,
    data: FileInfo,
}

#[derive(Clone)]
struct FileInfo {
    headers: Vec<(HeaderName, String)>,
    path: PathBuf,
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
    let file = &params.file;
    if let Some(info) = get_file_from_cache(file) {
        return Ok(info);
    }
    let dir = PathBuf::from(&params.dir);
    let mut path = dir.join(file);
    if path.to_string_lossy().len() < params.dir.len() {
        return Err(Error::Unknown {
            message: "access parent directory is not allowed".to_string(),
        });
    }

    let mut meta = fs::metadata(&path).await.map_err(|e| Error::Io {
        source: e,
        file: file.clone(),
    })?;
    let is_dir = meta.is_dir();
    if is_dir && !params.autoindex && params.index.is_empty() {
        return Err(Error::NotFound { file: file.clone() });
    }
    let mut headers = vec![];

    if is_dir && params.autoindex {
        let html = get_autoindex_html(path.as_path())?;
        headers.push((header::CONTENT_TYPE, "text/html".to_string()));
        headers.push((header::CACHE_CONTROL, "no-cache".to_string()));
        return Ok(FileInfo {
            headers,
            path,
            body: Some(html.into_bytes()),
        });
    }
    if is_dir && !params.index.is_empty() {
        path = path.join(&params.index);
        meta = fs::metadata(&path).await.map_err(|e| Error::Io {
            source: e,
            file: file.clone(),
        })?;
    }
    let content_type = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .to_string();
    let mut is_html = false;
    if content_type.contains("text/html") {
        is_html = true;
        headers.push((header::CACHE_CONTROL, "no-cache".to_string()));
    } else {
        headers.push((header::CACHE_CONTROL, params.cache_control.clone()));
    }

    headers.push((header::CONTENT_TYPE, content_type));

    let size = meta.size();
    // Generate ETag based on file size and modification time
    if let Ok(mod_time) = meta.modified() {
        let value = mod_time
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if value > 0 {
            let etag = format!(r#"W/"{:x}-{:x}""#, size, value);
            headers.push((header::ETAG, etag));
        }
    }
    let body = if size < 30 * 1024 {
        Some(fs::read(&path).await.map_err(|e| Error::Io {
            source: e,
            file: params.file.clone(),
        })?)
    } else {
        None
    };
    let info = FileInfo {
        headers,
        path,
        body,
    };
    if !is_html && info.body.is_some() {
        set_file_to_cache(file, &info);
    }

    Ok(info)
}

// 处理函数
pub async fn static_serve(params: StaticServeParams) -> Result<Response> {
    let file_info = get_file(&params).await?;

    let file = fs::OpenOptions::new()
        .read(true)
        .open(file_info.path)
        .await
        .map_err(|e| Error::Io {
            source: e,
            file: params.file.clone(),
        })?;

    let mut resp = if let Some(body) = file_info.body {
        body.into_response()
    } else {
        let stream = ReaderStream::new(file);
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
