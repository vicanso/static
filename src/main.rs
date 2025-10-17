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

use crate::error::{Error, Result, handle_error};
use crate::serve::X_ORIGINAL_SIZE_HEADER_NAME;
use axum::body::Body;
use axum::error_handling::HandleErrorLayer;
use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::http::{Request, Uri};
use axum::middleware::from_fn;
use axum::response::Response;
use axum::routing::get;
use axum::{Router, middleware::Next};
use config::Config;
use serve::{StaticServeParams, static_serve};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use substring::Substring;
use tokio::signal;
use tower::ServiceBuilder;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{NotForContentType, Predicate, SizeAbove};
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

mod config;
mod error;
mod serve;
mod storage;

static HEALTH_CHECK_RUNNING: AtomicBool = AtomicBool::new(true);

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
        info!("SIGTERM received, health check will return 500");
        HEALTH_CHECK_RUNNING.store(false, Ordering::Relaxed);
        // 等待5秒
        tokio::time::sleep(Duration::from_secs(5)).await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("signal received, starting graceful shutdown");
}

async fn run(config: Arc<Config>) {
    let app = Router::new()
        .route("/health", get(health_check))
        .fallback(get(serve))
        .with_state(config.clone());

    let builder = ServiceBuilder::new();
    let builder = builder
        .layer(from_fn(access_log))
        .layer(HandleErrorLayer::new(handle_error));
    let size = config.compress_min_length;
    let app = if size > 0 {
        let predicate = SizeAbove::new(size)
            .and(NotForContentType::GRPC)
            .and(NotForContentType::IMAGES)
            .and(NotForContentType::SSE);
        app.layer(
            builder
                .layer(CompressionLayer::new().compress_when(predicate))
                .timeout(config.timeout),
        )
    } else {
        app.layer(builder.timeout(config.timeout))
    };

    let listener = tokio::net::TcpListener::bind(&config.listen_addr)
        .await
        .unwrap();
    info!("server running on http://{}", config.listen_addr);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .unwrap();
}

#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub IpAddr);

impl<S> FromRequestParts<S> for ClientIp
where
    S: Sync,
{
    type Rejection = Error;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> std::result::Result<Self, Self::Rejection> {
        if let Some(x_forwarded_for) = parts.headers.get("X-Forwarded-For")
            && let Some(ip) = x_forwarded_for
                .to_str()
                .unwrap_or_default()
                .split(',')
                .next()
            && let Ok(ip) = ip.parse::<IpAddr>()
        {
            return Ok(ClientIp(ip));
        }
        if let Some(x_real_ip) = parts.headers.get("X-Real-Ip")
            && let Ok(ip) = x_real_ip.to_str().unwrap().parse::<IpAddr>()
        {
            return Ok(ClientIp(ip));
        }
        let ip = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip())
            .ok_or_else(|| Error::Unknown)?;
        Ok(ClientIp(ip))
    }
}

async fn access_log(ClientIp(ip): ClientIp, req: Request<Body>, next: Next) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();

    let user_agent = req
        .headers()
        .get("User-Agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();

    let start = Instant::now();
    let response = next.run(req).await;

    let size = response
        .headers()
        .get(X_ORIGINAL_SIZE_HEADER_NAME.as_str())
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");

    info!(
        target: "access_log",
        ip = %ip,
        method = %method,
        uri = %uri,
        status = response.status().as_u16(),
        size,
        duration = format!("{}ms", start.elapsed().as_millis()),
        user_agent,
    );

    response
}

enum HandleCategory {
    Normal,
    ExtHtml,
    IndexHtml,
}

// 处理函数
async fn serve(State(config): State<Arc<Config>>, uri: Uri) -> Result<Response> {
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

    let mut category_list = vec![HandleCategory::Normal];
    if config.fallback_html_404 {
        category_list.push(HandleCategory::ExtHtml);
    }
    if config.fallback_index_404 {
        category_list.push(HandleCategory::IndexHtml);
    }
    let mut err = Error::NotFound { file: file.clone() };

    for category in category_list {
        let current_file = match category {
            HandleCategory::Normal => file.clone(),
            HandleCategory::ExtHtml => format!("{file}.html"),
            HandleCategory::IndexHtml => config.index_file.clone(),
        };
        err = match static_serve(StaticServeParams {
            index: config.index_file.clone(),
            autoindex: config.autoindex,
            cache_control: config.cache_control.clone(),
            html_replaces: config.html_replaces.clone(),
            file: current_file,
            cache_size: config.cache_size,
            cache_ttl: config.cache_ttl,
        })
        .await
        {
            Ok(mut response) => {
                for (key, value) in config.response_headers.iter() {
                    response.headers_mut().insert(key, value.clone());
                }
                return Ok(response);
            }
            Err(e) => e,
        };
        if err.is_not_found() {
            continue;
        }
        return Err(err);
    }
    Err(err)
}

async fn health_check() -> (StatusCode, &'static str) {
    if HEALTH_CHECK_RUNNING.load(Ordering::Relaxed) {
        (StatusCode::OK, "healthy")
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, "unhealthy")
    }
}

fn init_logger() {
    let mut level = Level::INFO;
    if let Ok(log_level) = std::env::var("LOG_LEVEL")
        && let Ok(value) = Level::from_str(log_level.as_str())
    {
        level = value;
    }
    let timer = tracing_subscriber::fmt::time::OffsetTime::local_rfc_3339().unwrap_or_else(|_| {
        tracing_subscriber::fmt::time::OffsetTime::new(
            time::UtcOffset::from_hms(0, 0, 0).unwrap(),
            time::format_description::well_known::Rfc3339,
        )
    });
    let subscriber = FmtSubscriber::builder()
        .with_max_level(level)
        .with_timer(timer)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
}

fn main() {
    init_logger();
    let config = Arc::new(Config::new());
    let cpus = std::env::var("STATIC_THREADS")
        .map(|v| v.parse::<usize>().unwrap_or(num_cpus::get()))
        .unwrap_or(num_cpus::get())
        .max(1);
    info!(
        threads = cpus,
        config = ?config,
        "starting static server",
    );
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(cpus)
        .build()
        .unwrap()
        .block_on(run(config));
}
