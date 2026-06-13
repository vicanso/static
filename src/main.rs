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

use crate::error::{Error, Result, handle_error, init_error_template};
use crate::serve::X_ORIGINAL_SIZE_HEADER_NAME;
use axum::body::Body;
use axum::error_handling::HandleErrorLayer;
use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, Request, Uri, header};
use axum::middleware::from_fn;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, middleware::Next};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use config::Config;
use mimalloc::MiMalloc;
use serve::{StaticServeParams, static_serve};

// Route all allocations through mimalloc. On alloc/free-heavy workloads
// (header maps, Bytes, cache misses) this typically shaves a few percent off
// CPU vs the system glibc allocator and reduces fragmentation in long-running
// processes. Drop-in: no API surface changes.
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::signal;
use tower::ServiceBuilder;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{Predicate, SizeAbove};
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

mod config;
mod error;
mod metrics;
mod rate_limit;
mod serve;
mod storage;

static HEALTH_CHECK_RUNNING: AtomicBool = AtomicBool::new(true);

// Compression predicate: a default-deny whitelist of well-compressing content
// types, and never compress partial/range responses (compressing them would
// corrupt Content-Range / Content-Length and break range semantics).
// Pre-compressed `.br`/`.gz` already carry Content-Encoding so tower-http
// skips them regardless.
#[derive(Clone, Copy)]
struct Compressible;

impl Predicate for Compressible {
    fn should_compress<B>(&self, response: &axum::http::Response<B>) -> bool
    where
        B: http_body::Body,
    {
        if response.headers().contains_key(header::CONTENT_RANGE) {
            return false;
        }
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
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
}

async fn shutdown_signal(delay: Duration) {
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
        info!("SIGTERM received, health check will return 503");
        HEALTH_CHECK_RUNNING.store(false, Ordering::Relaxed);
        // Drain window: keep serving in-flight requests while /health reports
        // 503 (Service Unavailable — "temporarily down", which load balancers
        // treat as deregister-and-retry rather than the alert-worthy 500) so the
        // load balancer can deregister this instance.
        tokio::time::sleep(delay).await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    #[cfg(not(unix))]
    let _ = delay;

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("signal received, starting graceful shutdown");
}

async fn run(config: Arc<Config>) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut router = Router::new().route("/health", get(health_check));
    if config.metrics_enabled {
        router = router.route("/metrics", get(metrics_handler));
    }
    // `fallback(serve)` (not `get(serve)`) so OPTIONS reaches the handler for
    // CORS preflight; non-GET/HEAD/OPTIONS methods get a 405 there.
    let app = router.fallback(serve).with_state(config.clone());

    let builder = ServiceBuilder::new().layer(HandleErrorLayer::new(handle_error));
    let size = config.compress_min_length;
    let app = if size > 0 {
        let predicate = SizeAbove::new(size).and(Compressible);
        app.layer(
            builder
                .layer(
                    CompressionLayer::new()
                        .quality(config.compress_level)
                        .compress_when(predicate),
                )
                .timeout(config.timeout),
        )
    } else {
        app.layer(builder.timeout(config.timeout))
    };
    let app = if config.access_log {
        app.layer(from_fn(access_log))
    } else {
        app
    };
    // Outermost layer: records metrics and strips the internal
    // `x-original-size` header so it never reaches the client.
    let app = app.layer(from_fn(track_metrics));

    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    info!("server running on http://{}", config.listen_addr);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(config.shutdown_delay))
    .await?;
    Ok(())
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
            && let Ok(ip) = x_real_ip.to_str().unwrap_or_default().parse::<IpAddr>()
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
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(-1);

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

// Outermost middleware: record one metric sample per response and strip the
// internal `x-original-size` header (used only by access_log / metrics to know
// the pre-compression body size) so it is never exposed to clients.
async fn track_metrics(req: Request<Body>, next: Next) -> Response {
    // Only time the request when metrics are on — when off, skip the two clock
    // reads and the recording entirely. The header strip below always runs: it
    // is an internal marker that must never reach the client regardless.
    let start = metrics::enabled().then(Instant::now);
    let mut response = next.run(req).await;
    if let Some(start) = start {
        let bytes = response
            .headers()
            .get(X_ORIGINAL_SIZE_HEADER_NAME.as_str())
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        metrics::record_request(response.status().as_u16(), bytes, start.elapsed());
    }
    response
        .headers_mut()
        .remove(X_ORIGINAL_SIZE_HEADER_NAME.as_str());
    response
}

async fn metrics_handler() -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        metrics::render(),
    )
        .into_response()
}

fn method_not_allowed() -> Response {
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
    resp.headers_mut().insert(
        header::ALLOW,
        HeaderValue::from_static("GET, HEAD, OPTIONS"),
    );
    resp
}

// Resolve the `Access-Control-Allow-Origin` value for a request.
// Returns `(header_value, needs_vary_origin)`, or `None` when CORS is off or
// the origin is not allowed.
fn cors_origin(config: &Config, origin: Option<&str>) -> Option<(String, bool)> {
    let allow = config.cors_allow_origin.as_deref()?.trim();
    if allow == "*" {
        // "*" is invalid alongside credentials — echo the request origin.
        if config.cors_allow_credentials {
            return origin.map(|o| (o.to_string(), true));
        }
        return Some(("*".to_string(), false));
    }
    let mut first = None;
    let mut count = 0usize;
    for item in allow.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        count += 1;
        if first.is_none() {
            first = Some(item.to_string());
        }
        if origin == Some(item) {
            return Some((item.to_string(), true));
        }
    }
    // A single fixed origin is always advertised (no per-request variance).
    if count == 1 {
        return first.map(|o| (o, false));
    }
    None
}

// Apply custom response headers, the nosniff default, and CORS headers to a
// response just before it is returned.
fn apply_common_headers(resp: &mut Response, config: &Config, origin: Option<&str>) {
    for (key, value) in config.response_headers.iter() {
        resp.headers_mut().insert(key, value.clone());
    }
    if config.content_type_nosniff && !resp.headers().contains_key(header::X_CONTENT_TYPE_OPTIONS) {
        resp.headers_mut().insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
    }
    if let Some((acao, vary_origin)) = cors_origin(config, origin) {
        if let Ok(v) = HeaderValue::try_from(acao) {
            resp.headers_mut()
                .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
        }
        if config.cors_allow_credentials {
            resp.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                HeaderValue::from_static("true"),
            );
        }
        if vary_origin {
            resp.headers_mut()
                .append(header::VARY, HeaderValue::from_static("Origin"));
        }
    }
}

fn cors_preflight(config: &Config, origin: Option<&str>) -> Response {
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::NO_CONTENT;
    apply_common_headers(&mut resp, config, origin);
    if let Ok(v) = HeaderValue::try_from(config.cors_allow_methods.clone()) {
        resp.headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_METHODS, v);
    }
    if let Some(h) = &config.cors_allow_headers
        && let Ok(v) = HeaderValue::try_from(h.clone())
    {
        resp.headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_HEADERS, v);
    }
    if let Some(age) = &config.cors_max_age
        && let Ok(v) = HeaderValue::try_from(age.clone())
    {
        resp.headers_mut().insert(header::ACCESS_CONTROL_MAX_AGE, v);
    }
    resp
}

// 处理函数
async fn serve(
    State(config): State<Arc<Config>>,
    ClientIp(ip): ClientIp,
    method: Method,
    req_headers: HeaderMap,
    uri: Uri,
) -> Result<Response> {
    let path = uri.path();
    let is_head = method == Method::HEAD;

    // IP access control: blocklist takes priority, then allowlist
    if !config.ip_blocklist.is_empty() && config.ip_blocklist.iter().any(|net| net.contains(&ip)) {
        return Err(Error::Forbidden);
    }
    if !config.ip_allowlist.is_empty() && !config.ip_allowlist.iter().any(|net| net.contains(&ip)) {
        return Err(Error::Forbidden);
    }

    // Per-IP rate limiting (token bucket), after IP access control so blocked
    // IPs never allocate bucket state. No-op (and lock-free) when disabled.
    // IPs/CIDRs in STATIC_RATE_LIMIT_EXEMPT (e.g. trusted internal networks or
    // monitors) skip the limiter entirely. Like the 401/405 guards below, this
    // is a bare response — it does not go through the custom error page or
    // apply_common_headers.
    let rate_exempt = config.rate_limit_exempt.iter().any(|net| net.contains(&ip));
    if !rate_exempt && let Some(retry_after) = rate_limit::check(ip) {
        let mut resp = Response::new(Body::empty());
        *resp.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        if let Ok(v) = HeaderValue::try_from(retry_after.to_string()) {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        return Ok(resp);
    }

    let origin = req_headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string());

    // CORS preflight / method gating, before Basic Auth so a browser preflight
    // (which carries no credentials) is not rejected with 401.
    if method == Method::OPTIONS {
        if config.cors_allow_origin.is_some() {
            return Ok(cors_preflight(&config, origin.as_deref()));
        }
        return Ok(method_not_allowed());
    }
    if method != Method::GET && method != Method::HEAD {
        return Ok(method_not_allowed());
    }

    // Basic Auth
    if !config.basic_auth.is_empty() {
        let authorized = req_headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Basic "))
            .and_then(|b64| BASE64_STANDARD.decode(b64.trim()).ok())
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .is_some_and(|creds| config.basic_auth.contains(&creds));

        if !authorized {
            let mut resp = Response::new(Body::empty());
            *resp.status_mut() = StatusCode::UNAUTHORIZED;
            if let Ok(v) =
                HeaderValue::try_from(format!("Basic realm=\"{}\"", config.basic_auth_realm))
            {
                resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
            }
            return Ok(resp);
        }
    }

    // Check redirect rules before any file serving
    for (from, status, to) in config.redirects.iter() {
        if path == from.as_str() {
            let status_code =
                StatusCode::from_u16(*status).unwrap_or(StatusCode::MOVED_PERMANENTLY);
            let mut resp = Response::new(Body::empty());
            *resp.status_mut() = status_code;
            if let Ok(location) = HeaderValue::try_from(to.as_str()) {
                resp.headers_mut().insert(header::LOCATION, location);
            }
            apply_common_headers(&mut resp, &config, origin.as_deref());
            return Ok(resp);
        }
    }

    // Strip the leading '/' without allocating, then url-decode straight into an
    // owned String. `decode` returns a Cow that borrows when there is nothing to
    // decode, so `into_owned` allocates exactly once — vs the old path which
    // allocated for the substring and again for the decode.
    let path_no_slash = path.strip_prefix('/').unwrap_or(path);
    let file = match urlencoding::decode(path_no_slash) {
        Ok(decoded) => decoded.into_owned(),
        Err(_) => path_no_slash.to_string(),
    };

    let index = config.index_file.clone();
    // Header echoes are stored as Arc<str> so the fallback retry loop below
    // can share them by refcount instead of allocating new Strings per
    // iteration. They are also built once here, not inside the loop.
    let range: Option<Arc<str>> = req_headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(Arc::from);
    let if_range: Option<Arc<str>> = req_headers
        .get(header::IF_RANGE)
        .and_then(|v| v.to_str().ok())
        .map(Arc::from);
    let if_none_match: Option<Arc<str>> = if config.not_modified {
        req_headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .map(Arc::from)
    } else {
        None
    };
    let if_modified_since: Option<Arc<str>> = if config.not_modified {
        req_headers
            .get(header::IF_MODIFIED_SINCE)
            .and_then(|v| v.to_str().ok())
            .map(Arc::from)
    } else {
        None
    };
    let accept_encoding: Option<Arc<str>> = if config.precompressed {
        req_headers
            .get(header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .map(Arc::from)
    } else {
        None
    };
    let mut last_err = Error::NotFound { file: file.clone() };

    // Build params once outside the loop and reuse by reference. The fallback
    // retry loop typically runs once, so previously every Option<String> was
    // cloned (allocating) on each iteration even though only `file` differs.
    let mut params = StaticServeParams {
        index: index.clone(),
        autoindex: config.autoindex,
        cache_control: config.cache_control.clone(),
        cache_control_map: config.cache_control_map.clone(),
        html_replacer: config.html_replacer.clone(),
        file: String::new(),
        cache_size: config.cache_size,
        cache_ttl: config.cache_ttl,
        not_found_cache_ttl: config.not_found_cache_ttl,
        html_cache_ttl: config.html_cache_ttl,
        range,
        if_range,
        if_none_match,
        if_modified_since,
        accept_encoding,
        read_max_size: config.read_max_size,
        head: is_head,
        request_path: Arc::from(path),
        request_query: uri.query().map(Arc::from),
    };

    for current_file in [
        Some(file.clone()),
        config.fallback_html_404.then(|| format!("{file}.html")),
        config.fallback_index_404.then(|| index.to_string()),
    ]
    .into_iter()
    .flatten()
    {
        params.file = current_file;
        match static_serve(&params).await {
            Ok(mut response) => {
                apply_common_headers(&mut response, &config, origin.as_deref());
                return Ok(response);
            }
            Err(e) if e.is_not_found() => {
                last_err = e;
            }
            Err(e) => return Err(e),
        }
    }
    // Try serving custom error page (e.g., 404.html)
    if last_err.is_not_found()
        && let Ok(storage) = storage::get_storage()
        && let Ok(buf) = storage.dal.read("404.html").await
    {
        let mut resp = buf.to_vec().into_response();
        *resp.status_mut() = StatusCode::NOT_FOUND;
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        apply_common_headers(&mut resp, &config, origin.as_deref());
        return Ok(resp);
    }
    Err(last_err)
}

async fn health_check() -> (StatusCode, &'static str) {
    if HEALTH_CHECK_RUNNING.load(Ordering::Relaxed) {
        (StatusCode::OK, "healthy")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "unhealthy")
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
            time::UtcOffset::from_hms(0, 0, 0).unwrap_or(time::UtcOffset::UTC),
            time::format_description::well_known::Rfc3339,
        )
    });
    let builder = FmtSubscriber::builder()
        .with_max_level(level)
        .with_timer(timer);
    let json = std::env::var("LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    if json {
        tracing::subscriber::set_global_default(builder.json().finish())
            .expect("setting default subscriber failed");
    } else {
        tracing::subscriber::set_global_default(builder.finish())
            .expect("setting default subscriber failed");
    }
}

fn main() {
    init_logger();
    let config = Arc::new(Config::new());
    init_error_template(config.error_page.as_deref());
    rate_limit::init(config.rate_limit, config.rate_limit_burst);
    storage::init_backend_resilience(
        config.backend_retry_max,
        config.backend_timeout,
        config.backend_io_timeout,
    );
    metrics::set_enabled(config.metrics_enabled);
    metrics::set_cache_capacity(config.cache_size);
    info!(
        config = ?config,
        "starting static server",
    );
    let _ = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(config.threads)
        .build()
        .unwrap_or_else(|e| panic!("failed to build tokio runtime: {}", e))
        .block_on(run(config));
}
