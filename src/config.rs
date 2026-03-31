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
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, warn};

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
    pub html_replaces: Arc<Vec<(Vec<u8>, Vec<u8>)>>,
    pub response_headers: HeaderMap,
    pub cache_size: usize,
    pub cache_ttl: Duration,
    pub not_modified: bool,
    pub precompressed: bool,
    pub access_log: bool,
    pub read_max_size: u64,
    pub threads: usize,
}

fn deserialize_humantime<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    humantime::parse_duration(&s).map_err(serde::de::Error::custom)
}

fn deserialize_bytesize<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    s.parse::<bytesize::ByteSize>()
        .map(|b| b.0)
        .map_err(serde::de::Error::custom)
}

#[derive(Deserialize, Debug)]
#[serde(default)]
struct EnvConfig {
    #[serde(deserialize_with = "deserialize_humantime")]
    timeout: Duration,
    compress_min_length: u16,
    index_file: String,
    autoindex: bool,
    listen_addr: String,
    cache_control: String,
    fallback_index_404: bool,
    fallback_html_404: bool,
    cache_size: usize,
    #[serde(deserialize_with = "deserialize_humantime")]
    cache_ttl: Duration,
    not_modified: bool,
    precompressed: bool,
    access_log: bool,
    #[serde(deserialize_with = "deserialize_bytesize")]
    read_max_size: u64,
    threads: usize,
}

impl Default for EnvConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            compress_min_length: 256,
            index_file: "index.html".to_string(),
            autoindex: false,
            listen_addr: "0.0.0.0:3000".to_string(),
            cache_control: "public, max-age=31536000, immutable".to_string(),
            fallback_index_404: false,
            fallback_html_404: false,
            cache_size: 1024,
            cache_ttl: Duration::from_secs(10 * 60),
            not_modified: false,
            precompressed: false,
            access_log: true,
            read_max_size: bytesize::ByteSize::kb(250).0,
            threads: num_cpus::get(),
        }
    }
}

impl Config {
    pub fn new() -> Self {
        let env_cfg = match envy::prefixed("STATIC_").from_env::<EnvConfig>() {
            Ok(cfg) => cfg,
            Err(e) => {
                error!(
                    "Failed to parse static configs from env: {}. Using defaults.",
                    e
                );
                EnvConfig::default()
            }
        };

        let mut html_replaces = vec![];
        let mut response_headers = HeaderMap::new();
        let replace_prefix = "STATIC_HTML_REPLACE_";
        let header_prefix = "STATIC_RESPONSE_HEADER_";

        for (key, value) in std::env::vars() {
            if let Some(stripped_key) = key.strip_prefix(replace_prefix) {
                html_replaces.push((stripped_key.as_bytes().to_vec(), value.as_bytes().to_vec()));
            } else if let Some(stripped_key) = key.strip_prefix(header_prefix) {
                let header_name = stripped_key.replace('_', "-").to_lowercase();
                if let (Ok(name), Ok(val)) = (
                    HeaderName::try_from(header_name),
                    HeaderValue::try_from(value.clone()),
                ) {
                    response_headers.insert(name, val);
                } else {
                    warn!("Invalid dynamic header format: {}={}", key, value);
                }
            }
        }

        Self {
            timeout: env_cfg.timeout,
            compress_min_length: env_cfg.compress_min_length,
            index_file: env_cfg.index_file,
            autoindex: env_cfg.autoindex,
            listen_addr: env_cfg.listen_addr,
            cache_control: env_cfg.cache_control,
            fallback_index_404: env_cfg.fallback_index_404,
            fallback_html_404: env_cfg.fallback_html_404,
            cache_size: env_cfg.cache_size,
            cache_ttl: env_cfg.cache_ttl,
            not_modified: env_cfg.not_modified,
            precompressed: env_cfg.precompressed,
            access_log: env_cfg.access_log,
            read_max_size: env_cfg.read_max_size,
            html_replaces: Arc::new(html_replaces),
            response_headers,
            threads: env_cfg.threads.max(1),
        }
    }
}
