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

use crate::serve::HtmlReplacer;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use ipnet::IpNet;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, warn};

#[derive(Debug, Clone)]
pub struct Config {
    pub timeout: Duration,
    pub compress_min_length: u16,
    pub index_file: Arc<str>,
    pub autoindex: bool,
    pub listen_addr: String,
    pub cache_control: Arc<str>,
    pub fallback_index_404: bool,
    pub fallback_html_404: bool,
    // Pre-built once at startup; None when no STATIC_HTML_REPLACE_* is set.
    pub html_replacer: Option<Arc<HtmlReplacer>>,
    pub cache_control_map: Arc<HashMap<String, String>>,
    pub redirects: Arc<Vec<(String, u16, String)>>,
    pub ip_allowlist: Arc<Vec<IpNet>>,
    pub ip_blocklist: Arc<Vec<IpNet>>,
    pub basic_auth: Arc<HashSet<String>>,
    pub basic_auth_realm: String,
    pub error_page: Option<String>,
    pub response_headers: HeaderMap,
    pub cache_size: usize,
    pub cache_ttl: Duration,
    pub not_modified: bool,
    pub precompressed: bool,
    pub access_log: bool,
    pub read_max_size: u64,
    pub threads: usize,
    pub content_type_nosniff: bool,
    pub shutdown_delay: Duration,
    pub metrics_enabled: bool,
    pub cors_allow_origin: Option<String>,
    pub cors_allow_methods: String,
    pub cors_allow_headers: Option<String>,
    pub cors_max_age: Option<String>,
    pub cors_allow_credentials: bool,
}

// Parse an optional humantime duration from env. Absent -> default;
// present but invalid -> log and exit (never run with a misread config).
fn parse_duration_or_exit(name: &str, raw: Option<&str>, default: Duration) -> Duration {
    match raw {
        None => default,
        Some(s) => humantime::parse_duration(s).unwrap_or_else(|e| {
            error!("Invalid {name}={s}: {e}");
            std::process::exit(1)
        }),
    }
}

// Parse an optional byte size from env. Absent -> default;
// present but invalid -> log and exit.
fn parse_bytesize_or_exit(name: &str, raw: Option<&str>, default: u64) -> u64 {
    match raw {
        None => default,
        Some(s) => s
            .parse::<bytesize::ByteSize>()
            .map(|b| b.0)
            .unwrap_or_else(|e| {
                error!("Invalid {name}={s}: {e}");
                std::process::exit(1)
            }),
    }
}

#[derive(Deserialize, Debug)]
#[serde(default)]
struct EnvConfig {
    timeout: Option<String>,
    compress_min_length: u16,
    index_file: String,
    autoindex: bool,
    listen_addr: String,
    cache_control: String,
    fallback_index_404: bool,
    fallback_html_404: bool,
    cache_size: usize,
    cache_ttl: Option<String>,
    not_modified: bool,
    precompressed: bool,
    access_log: bool,
    read_max_size: Option<String>,
    threads: usize,
    ip_allowlist: String,
    ip_blocklist: String,
    basic_auth_realm: String,
    error_page: Option<String>,
    content_type_nosniff: bool,
    shutdown_delay: Option<String>,
    metrics: bool,
    cors_allow_origin: Option<String>,
    cors_allow_methods: String,
    cors_allow_headers: Option<String>,
    cors_max_age: Option<String>,
    cors_allow_credentials: bool,
}

impl Default for EnvConfig {
    fn default() -> Self {
        Self {
            timeout: None,
            compress_min_length: 256,
            index_file: "index.html".to_string(),
            autoindex: false,
            listen_addr: "0.0.0.0:3000".to_string(),
            cache_control: "public, max-age=31536000, immutable".to_string(),
            fallback_index_404: false,
            fallback_html_404: false,
            cache_size: 1024,
            cache_ttl: None,
            not_modified: false,
            precompressed: false,
            access_log: true,
            read_max_size: None,
            threads: num_cpus::get(),
            ip_allowlist: String::new(),
            ip_blocklist: String::new(),
            basic_auth_realm: "static".to_string(),
            error_page: None,
            content_type_nosniff: true,
            shutdown_delay: None,
            metrics: true,
            cors_allow_origin: None,
            cors_allow_methods: "GET, HEAD, OPTIONS".to_string(),
            cors_allow_headers: None,
            cors_max_age: None,
            cors_allow_credentials: false,
        }
    }
}

fn parse_ip_list(s: &str) -> Vec<IpNet> {
    if s.is_empty() {
        return vec![];
    }
    s.split(',')
        .filter_map(|item| {
            let item = item.trim();
            item.parse::<IpNet>()
                .or_else(|_| item.parse::<IpAddr>().map(IpNet::from))
                .ok()
        })
        .collect()
}

impl Config {
    pub fn new() -> Self {
        let env_cfg = match envy::prefixed("STATIC_").from_env::<EnvConfig>() {
            Ok(cfg) => cfg,
            Err(e) => {
                error!("Failed to parse static configs from env: {e}");
                std::process::exit(1)
            }
        };

        let mut html_replaces = vec![];
        let mut response_headers = HeaderMap::new();
        let mut cache_control_map = HashMap::new();
        let mut redirects = Vec::new();
        let mut basic_auth = HashSet::new();
        let replace_prefix = "STATIC_HTML_REPLACE_";
        let header_prefix = "STATIC_RESPONSE_HEADER_";
        let cache_control_ext_prefix = "STATIC_CACHE_CONTROL_EXT_";
        let redirect_prefix = "STATIC_REDIRECT_";
        let basic_auth_prefix = "STATIC_BASIC_AUTH_";

        for (key, value) in std::env::vars() {
            if let Some(stripped_key) = key.strip_prefix(replace_prefix) {
                html_replaces.push((stripped_key.as_bytes().to_vec(), value.as_bytes().to_vec()));
            } else if let Some(ext) = key.strip_prefix(cache_control_ext_prefix) {
                cache_control_map.insert(ext.to_lowercase(), value);
            } else if let Some(key) = key.strip_prefix(basic_auth_prefix) {
                // REALM is handled by envy via basic_auth_realm field; skip it here
                if key != "REALM" {
                    basic_auth.insert(value);
                }
            } else if key.starts_with(redirect_prefix) {
                // Format: "<source> <target>" or "<source> <status_code> <target>"
                if let Some((from, rest)) = value.split_once(' ') {
                    let (status, to) = if let Some((s, t)) = rest.split_once(' ')
                        && let Ok(code) = s.parse::<u16>()
                    {
                        (code, t.to_string())
                    } else {
                        (301u16, rest.to_string())
                    };
                    redirects.push((from.to_string(), status, to));
                } else {
                    warn!("Invalid redirect rule (missing target): {}={}", key, value);
                }
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

        if let Some(ref v) = env_cfg.cors_max_age
            && !v.chars().all(|c| c.is_ascii_digit())
        {
            error!("Invalid STATIC_CORS_MAX_AGE={v}: must be an integer number of seconds");
            std::process::exit(1)
        }

        Self {
            timeout: parse_duration_or_exit(
                "STATIC_TIMEOUT",
                env_cfg.timeout.as_deref(),
                Duration::from_secs(30),
            ),
            compress_min_length: env_cfg.compress_min_length,
            index_file: env_cfg.index_file.into(),
            autoindex: env_cfg.autoindex,
            listen_addr: env_cfg.listen_addr,
            cache_control: env_cfg.cache_control.into(),
            fallback_index_404: env_cfg.fallback_index_404,
            fallback_html_404: env_cfg.fallback_html_404,
            cache_size: env_cfg.cache_size,
            cache_ttl: parse_duration_or_exit(
                "STATIC_CACHE_TTL",
                env_cfg.cache_ttl.as_deref(),
                Duration::from_secs(10 * 60),
            ),
            not_modified: env_cfg.not_modified,
            precompressed: env_cfg.precompressed,
            access_log: env_cfg.access_log,
            read_max_size: parse_bytesize_or_exit(
                "STATIC_READ_MAX_SIZE",
                env_cfg.read_max_size.as_deref(),
                bytesize::ByteSize::kb(250).0,
            ),
            html_replacer: HtmlReplacer::new(html_replaces).map(Arc::new),
            cache_control_map: Arc::new(cache_control_map),
            redirects: Arc::new(redirects),
            ip_allowlist: Arc::new(parse_ip_list(&env_cfg.ip_allowlist)),
            ip_blocklist: Arc::new(parse_ip_list(&env_cfg.ip_blocklist)),
            basic_auth: Arc::new(basic_auth),
            basic_auth_realm: env_cfg.basic_auth_realm,
            error_page: env_cfg.error_page,
            response_headers,
            threads: env_cfg.threads.max(1),
            content_type_nosniff: env_cfg.content_type_nosniff,
            shutdown_delay: parse_duration_or_exit(
                "STATIC_SHUTDOWN_DELAY",
                env_cfg.shutdown_delay.as_deref(),
                Duration::from_secs(5),
            ),
            metrics_enabled: env_cfg.metrics,
            cors_allow_origin: env_cfg.cors_allow_origin.filter(|v| !v.trim().is_empty()),
            cors_allow_methods: env_cfg.cors_allow_methods,
            cors_allow_headers: env_cfg.cors_allow_headers.filter(|v| !v.trim().is_empty()),
            cors_max_age: env_cfg.cors_max_age,
            cors_allow_credentials: env_cfg.cors_allow_credentials,
        }
    }
}
