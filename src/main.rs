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

use crate::error::{handle_error, Result};
use axum::error_handling::HandleErrorLayer;
use axum::http::Uri;
use axum::response::Response;
use axum::{routing::get, Router};
use serve::{static_serve, StaticServeParams};
use std::sync::LazyLock;
use std::time::Duration;
use substring::Substring;
use tower::ServiceBuilder;
use tower_http::compression::predicate::SizeAbove;
use tower_http::compression::CompressionLayer;
use tracing::info;

mod error;
mod serve;
mod storage;

static STATIC_TIMEOUT: LazyLock<Duration> = LazyLock::new(|| {
    let timeout = std::env::var("STATIC_TIMEOUT").unwrap_or("30s".to_string());
    humantime::parse_duration(&timeout).unwrap_or(Duration::from_secs(30))
});

static STATIC_COMPRESS_MIN_LENGTH: LazyLock<u16> = LazyLock::new(|| {
    let min_length = std::env::var("STATIC_COMPRESS_MIN_LENGTH").unwrap_or("256".to_string());
    min_length.parse::<u16>().unwrap_or(256)
});

static STATIC_INDEX_FILE: LazyLock<String> =
    LazyLock::new(|| std::env::var("STATIC_INDEX_FILE").unwrap_or("index.html".to_string()));

static STATIC_AUTOINDEX: LazyLock<bool> = LazyLock::new(|| {
    let autoindex = std::env::var("STATIC_AUTOINDEX").unwrap_or("false".to_string());
    autoindex.parse::<bool>().unwrap_or(false)
});

static STATIC_LISTEN_ADDR: LazyLock<String> =
    LazyLock::new(|| std::env::var("STATIC_LISTEN_ADDR").unwrap_or("0.0.0.0:3000".to_string()));

static STATIC_CACHE_CONTROL: LazyLock<String> = LazyLock::new(|| {
    std::env::var("STATIC_CACHE_CONTROL")
        .unwrap_or("public, max-age=31536000, immutable".to_string())
});

static STATIC_HTML_REPLACES: LazyLock<Vec<(Vec<u8>, Vec<u8>)>> = LazyLock::new(|| {
    let prefix = "STATIC_HTML_REPLACE_";
    let mut values = vec![];
    for (key, value) in std::env::vars() {
        if key.starts_with(prefix) {
            let key = key.substring(prefix.len(), key.len());
            values.push((key.as_bytes().to_vec(), value.as_bytes().to_vec()));
        }
    }
    values
});

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let app = Router::new()
        .route("/health", get(health_check))
        .fallback(get(serve));

    let builder = ServiceBuilder::new();
    let builder = builder.layer(HandleErrorLayer::new(handle_error));
    let size = *STATIC_COMPRESS_MIN_LENGTH;
    let app = if size > 0 {
        app.layer(
            builder
                .layer(CompressionLayer::new().compress_when(SizeAbove::new(size)))
                .timeout(*STATIC_TIMEOUT),
        )
    } else {
        app.layer(builder.timeout(*STATIC_TIMEOUT))
    };

    let listener = tokio::net::TcpListener::bind(STATIC_LISTEN_ADDR.as_str())
        .await
        .unwrap();
    info!("Server running on http://{}", STATIC_LISTEN_ADDR.as_str());

    axum::serve(listener, app).await.unwrap();
}

// 处理函数
async fn serve(uri: Uri) -> Result<Response> {
    let path = uri.path();
    let file = if !path.is_empty() {
        path.substring(1, path.len()).to_string()
    } else {
        path.to_string()
    };
    let file = if let Ok(file) = urlencoding::decode(&file) {
        file.to_string()
    } else {
        file
    };
    static_serve(StaticServeParams {
        index: STATIC_INDEX_FILE.clone(),
        autoindex: *STATIC_AUTOINDEX,
        cache_control: STATIC_CACHE_CONTROL.clone(),
        html_replaces: STATIC_HTML_REPLACES.clone(),
        file,
    })
    .await
}

async fn health_check() -> &'static str {
    "OK"
}
