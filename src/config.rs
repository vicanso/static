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

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub timeout: Duration,
    pub compress_min_length: u16,
    pub index_file: String,
    pub autoindex: bool,
    pub listen_addr: String,
    pub cache_control: String,
    pub fallback_index_404: bool,
    pub fallback_html_404: bool,
    pub html_replaces: Vec<(Vec<u8>, Vec<u8>)>,
    pub response_headers: HeaderMap,
    pub cache_size: usize,
    pub cache_ttl: Duration,
}

impl Config {
    pub fn new() -> Self {
        let mut html_replaces = vec![];
        let mut response_headers = HeaderMap::new();

        let replace_prefix = "STATIC_HTML_REPLACE_";
        let header_prefix = "STATIC_RESPONSE_HEADER_";

        for (key, value) in std::env::vars() {
            if let Some(stripped_key) = key.strip_prefix(replace_prefix) {
                html_replaces.push((stripped_key.as_bytes().to_vec(), value.as_bytes().to_vec()));
            } else if let Some(stripped_key) = key.strip_prefix(header_prefix) {
                let header_name = stripped_key.replace('_', "-");
                if let (Ok(name), Ok(val)) = (
                    HeaderName::try_from(header_name),
                    HeaderValue::try_from(value),
                ) {
                    response_headers.insert(name, val);
                }
            }
        }

        Self {
            timeout: humantime::parse_duration(
                &std::env::var("STATIC_TIMEOUT").unwrap_or_default(),
            )
            .unwrap_or(Duration::from_secs(30)),
            compress_min_length: std::env::var("STATIC_COMPRESS_MIN_LENGTH")
                .unwrap_or_default()
                .parse()
                .unwrap_or(256),
            index_file: std::env::var("STATIC_INDEX_FILE").unwrap_or("index.html".to_string()),
            autoindex: std::env::var("STATIC_AUTOINDEX")
                .unwrap_or_default()
                .parse()
                .unwrap_or(false),
            listen_addr: std::env::var("STATIC_LISTEN_ADDR").unwrap_or("0.0.0.0:3000".to_string()),
            cache_control: std::env::var("STATIC_CACHE_CONTROL")
                .unwrap_or("public, max-age=31536000, immutable".to_string()),
            fallback_index_404: std::env::var("STATIC_FALLBACK_INDEX_404")
                .unwrap_or_default()
                .parse()
                .unwrap_or(false),
            fallback_html_404: std::env::var("STATIC_FALLBACK_HTML_404")
                .unwrap_or_default()
                .parse()
                .unwrap_or(false),
            html_replaces,
            response_headers,
            cache_size: std::env::var("STATIC_CACHE_SIZE")
                .unwrap_or_default()
                .parse()
                .unwrap_or(1024),
            cache_ttl: humantime::parse_duration(
                &std::env::var("STATIC_CACHE_TTL").unwrap_or_default(),
            )
            .unwrap_or(Duration::from_secs(10 * 60)),
        }
    }
}
