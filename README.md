# static

**English** | [中文](./README_zh.md)

A high-performance static file server built with Rust and [Axum](https://github.com/tokio-rs/axum).

It uses [Apache OpenDAL](https://github.com/apache/opendal) as a unified storage abstraction layer — one consistent API for every backend, so switching storage never means switching libraries or rewriting code. Local filesystem, Amazon S3 (and S3-compatible services), FTP, and MongoDB GridFS all work out of the box.

## Features

- Multiple storage backends via OpenDAL — S3, FTP, GridFS, local filesystem
- Built-in gzip / brotli / zstd compression, plus pre-compressed file support (`.br` / `.zst` / `.gz`)
- TinyUFO LRU cache with configurable size and TTL
- HTTP Range requests (206 Partial Content) with `If-Range`, including multiple ranges via `multipart/byteranges`, for resumable downloads and media seeking
- Optional 304 Not Modified via ETag / If-None-Match
- Directory auto-indexing
- Dynamic HTML content replacement
- Custom response headers, with secure `X-Content-Type-Options: nosniff` by default
- CORS support, including preflight handling
- SPA fallback mode
- Basic Auth and IP allow / block lists
- Per-IP rate limiting (token bucket) for DoS resistance
- URL redirect rules
- Graceful shutdown with a configurable SIGTERM drain window
- Access logging (text or JSON) and a Prometheus `/metrics` endpoint

## Quick Start

```bash
docker run -d --restart=always \
    -p 3000:3000 \
    --name static \
    -v ./static:/static:ro \
    vicanso/static
```

This serves the contents of `./static` on port `3000`. To enable directory browsing, add `-e STATIC_AUTOINDEX=true`. Everything else is configured through environment variables — see below.

## Configuration

Every option is set via an environment variable and parsed once at startup.

### Core

| Variable | Default | Description |
|---|---|---|
| `STATIC_PATH` | `/static` | Storage path or URL. The backend is auto-detected from the scheme — see [Storage Backends](#storage-backends). |
| `STATIC_LISTEN_ADDR` | `0.0.0.0:3000` | Listen address |
| `STATIC_THREADS` | num CPUs | Tokio worker threads |
| `STATIC_TIMEOUT` | `30s` | Request timeout |
| `STATIC_SHUTDOWN_DELAY` | `5s` | SIGTERM drain window: `/health` returns 503 for this long before shutdown |

### Caching

| Variable | Default | Description |
|---|---|---|
| `STATIC_CACHE_CONTROL` | `public, max-age=31536000, immutable` | `Cache-Control` for static assets. HTML is always `no-cache`. |
| `STATIC_CACHE_CONTROL_EXT_*` | — | Per-extension override, e.g. `STATIC_CACHE_CONTROL_EXT_WASM=no-cache`. See [Per-Extension Cache Control](#per-extension-cache-control). |
| `STATIC_CACHE_SIZE` | `1024` | LRU cache entry count |
| `STATIC_CACHE_TTL` | `10m` | Cache TTL. HTML files are never cached. |
| `STATIC_NOT_MODIFIED` | `false` | Enable `304 Not Modified` via `If-None-Match` / `ETag` |

### Compression

| Variable | Default | Description |
|---|---|---|
| `STATIC_COMPRESS_MIN_LENGTH` | `256` | Minimum response size in bytes to compress (`0` disables the runtime compression layer entirely) |
| `STATIC_COMPRESS_LEVEL` | `default` | Runtime compression quality: `fastest`, `best`, `default`, or an integer for a precise per-algorithm level. Use `fastest` to cut CPU on high-traffic text/JS/JSON responses; prefer `STATIC_PRECOMPRESSED` to avoid runtime compression altogether. |
| `STATIC_PRECOMPRESSED` | `false` | Serve `.br` / `.zst` / `.gz` siblings (e.g. `app.js.br` for `app.js`) when the client supports the encoding, skipping runtime compression. Negotiation is `q`-value aware (a `br;q=0` is honored as a refusal). Negotiated responses are cached per-encoding, so a repeat hit serves straight from memory. |

### Routing & Fallback

| Variable | Default | Description |
|---|---|---|
| `STATIC_INDEX_FILE` | `index.html` | Directory index filename |
| `STATIC_AUTOINDEX` | `false` | Enable directory listing |
| `STATIC_FALLBACK_INDEX_404` | `false` | Serve the index file for unmatched routes (SPA mode) |
| `STATIC_FALLBACK_HTML_404` | `false` | Retry with an appended `.html` for unmatched routes |
| `STATIC_REDIRECT_*` | — | URL redirect rules. See [Redirect Rules](#redirect-rules). |

### Access Control

| Variable | Default | Description |
|---|---|---|
| `STATIC_BASIC_AUTH_*` | — | Basic Auth credentials, `STATIC_BASIC_AUTH_<NAME>=user:pass`. See [Basic Authentication](#basic-authentication). |
| `STATIC_BASIC_AUTH_REALM` | `static` | Realm string in the `WWW-Authenticate` header |
| `STATIC_IP_ALLOWLIST` | — | Comma-separated allowed IPs / CIDRs. See [IP Access Control](#ip-access-control). |
| `STATIC_IP_BLOCKLIST` | — | Comma-separated blocked IPs / CIDRs (checked before the allowlist) |
| `STATIC_RATE_LIMIT` | `0` | Per-IP rate limit in requests/sec (`0` disables). See [Rate Limiting](#rate-limiting). |
| `STATIC_RATE_LIMIT_BURST` | — | Token-bucket burst capacity (defaults to `STATIC_RATE_LIMIT`) |
| `STATIC_RATE_LIMIT_EXEMPT` | — | Comma-separated IPs / CIDRs exempt from rate limiting |

### Content & Headers

| Variable | Default | Description |
|---|---|---|
| `STATIC_HTML_REPLACE_*` | — | Replace HTML content, e.g. `STATIC_HTML_REPLACE_{{HOST}}=https://test.com` replaces `{{HOST}}` with `https://test.com` |
| `STATIC_RESPONSE_HEADER_*` | — | Add a custom response header, e.g. `STATIC_RESPONSE_HEADER_X_FRAME_OPTIONS=DENY` adds `x-frame-options: DENY` to every response |
| `STATIC_ERROR_PAGE` | — | Filesystem path to a custom error page template for all error statuses (uses `{{STATUS}}` / `{{REASON}}`). See [Custom Error Pages](#custom-error-pages). If set but unreadable, the server exits at startup. |
| `STATIC_CONTENT_TYPE_NOSNIFF` | `true` | Add `X-Content-Type-Options: nosniff` to responses (skipped if you set that header yourself via `STATIC_RESPONSE_HEADER_*`) |

### CORS

| Variable | Default | Description |
|---|---|---|
| `STATIC_CORS_ALLOW_ORIGIN` | — | Enables CORS. `*`, or a comma-separated origin allowlist (the request `Origin` is echoed back when it matches). Unset = CORS disabled. See [CORS](#cors). |
| `STATIC_CORS_ALLOW_METHODS` | `GET, HEAD, OPTIONS` | `Access-Control-Allow-Methods` for preflight responses |
| `STATIC_CORS_ALLOW_HEADERS` | — | `Access-Control-Allow-Headers` for preflight responses |
| `STATIC_CORS_MAX_AGE` | — | `Access-Control-Max-Age` in seconds (integer). Invalid value exits at startup. |
| `STATIC_CORS_ALLOW_CREDENTIALS` | `false` | Add `Access-Control-Allow-Credentials: true` (forces echoing the origin instead of `*`) |

### Observability

| Variable | Default | Description |
|---|---|---|
| `STATIC_METRICS` | `true` | Expose Prometheus metrics at `GET /metrics`; `false` also disables per-request collection (zero overhead). See [Metrics](#metrics). |

### I/O & Logging

| Variable | Default | Description |
|---|---|---|
| `STATIC_READ_MAX_SIZE` | `250KB` | Max file size buffered in memory; larger files are streamed. Accepts human-readable sizes (`30KB`, `1MB`). |
| `STATIC_DISABLE_SYMLINK_CHECK` | `false` | Local FS only. Skip the per-request `canonicalize()` syscall that blocks symlinks escaping the root. Lexical `../` traversal protection stays on regardless. Enable only when the asset tree is known to be symlink-free, to save the syscall on cache misses. |
| `STATIC_ACCESS_LOG` | `true` | Enable access logging |
| `LOG_LEVEL` | `INFO` | Log level: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` |
| `LOG_FORMAT` | `text` | Log output format: `text` or `json` |

## Basic Authentication

Protect the server with HTTP Basic Auth by setting one or more `STATIC_BASIC_AUTH_*` variables. The `<NAME>` suffix is an arbitrary label that distinguishes multiple accounts.

```bash
# Single account
STATIC_BASIC_AUTH_ADMIN=admin:secret

# Multiple accounts
STATIC_BASIC_AUTH_ALICE=alice:pass1
STATIC_BASIC_AUTH_BOB=bob:pass2

# Custom realm shown in the browser login dialog (default: "static")
STATIC_BASIC_AUTH_REALM=My Internal Tool
```

When any credential is set, unauthenticated requests receive a `401` with a `WWW-Authenticate` header. The `/health` endpoint always bypasses Basic Auth so load balancers can reach it.

## IP Access Control

Restrict access by client IP using an allowlist and a blocklist. Both accept individual IPs and CIDR notation, in IPv4 or IPv6. The client IP is resolved from `X-Forwarded-For`, then `X-Real-IP`, then the connection address.

```bash
# Block specific IPs and ranges
STATIC_IP_BLOCKLIST=1.2.3.4,10.0.0.0/8

# Only allow internal network access
STATIC_IP_ALLOWLIST=192.168.0.0/16,127.0.0.1

# Both can be set at once — the blocklist is checked first
STATIC_IP_BLOCKLIST=1.2.3.4
STATIC_IP_ALLOWLIST=192.168.0.0/16
```

Rejected requests receive a `403`. The `/health` endpoint always bypasses IP access control.

## Rate Limiting

Protect against bursts and DoS with a per-IP [token bucket](https://en.wikipedia.org/wiki/Token_bucket). It is **disabled by default**; set `STATIC_RATE_LIMIT` to the sustained number of requests per second allowed per client IP. `STATIC_RATE_LIMIT_BURST` is the bucket capacity — how many requests can arrive at once before the sustained rate applies — and defaults to `STATIC_RATE_LIMIT` when unset.

```bash
# Allow 50 req/s per IP, tolerating short bursts of up to 100
STATIC_RATE_LIMIT=50
STATIC_RATE_LIMIT_BURST=100

# Exempt trusted networks (internal services, monitoring) from the limit
STATIC_RATE_LIMIT_EXEMPT=10.0.0.0/8,192.168.0.0/16,127.0.0.1
```

The client IP is resolved the same way as IP access control (`X-Forwarded-For`, then `X-Real-IP`, then the connection address), so place the server behind a trusted proxy that sets these headers if you are not terminating connections directly. Requests that exceed the limit receive `429 Too Many Requests` with a `Retry-After` header (seconds). Limiting is applied after IP access control, and the `/health` and `/metrics` routes bypass it entirely.

Clients whose IP matches `STATIC_RATE_LIMIT_EXEMPT` (individual IPs or CIDR ranges, IPv4 or IPv6) skip the limiter — useful for internal networks, health checkers, or trusted upstreams that should never be throttled.

## Redirect Rules

Configure URL redirects via `STATIC_REDIRECT_*`. The `<NAME>` suffix is an arbitrary label that distinguishes multiple rules.

```bash
# Default 301 (permanent redirect)
STATIC_REDIRECT_HOME=/home /index.html
STATIC_REDIRECT_OLD_DOCS=/docs /v2/docs

# Explicit status code
STATIC_REDIRECT_API=/api 302 https://api.example.com
STATIC_REDIRECT_LEGACY=/old-product 301 https://example.com/new-product
```

Format: `STATIC_REDIRECT_<NAME>=<source_path> [status_code] <target>`

- `source_path` — exact path to match (e.g. `/old/page`)
- `status_code` — optional HTTP status code, defaults to `301`
- `target` — destination, either a path or a full URL

Redirects are evaluated before any file serving, and custom response headers (`STATIC_RESPONSE_HEADER_*`) are applied to redirect responses too.

## Per-Extension Cache Control

By default all non-HTML files use `STATIC_CACHE_CONTROL`. Override it per extension with `STATIC_CACHE_CONTROL_EXT_*` — the extension (without the dot) is the variable-name suffix, case-insensitive.

```bash
# Disable caching for WebAssembly (frequently updated in development)
STATIC_CACHE_CONTROL_EXT_WASM=no-cache

# Cache fonts for a year without immutable (allows re-validation)
STATIC_CACHE_CONTROL_EXT_WOFF2=public, max-age=31536000
STATIC_CACHE_CONTROL_EXT_WOFF=public, max-age=31536000

# Short cache for JSON data files
STATIC_CACHE_CONTROL_EXT_JSON=public, max-age=300
```

Priority, highest to lowest:

1. HTML files — always `no-cache`
2. `Cache-Control` returned by the storage backend
3. `STATIC_CACHE_CONTROL_EXT_<EXT>` — per-extension override
4. `STATIC_CACHE_CONTROL` — global default

## Custom Error Pages

Two independent mechanisms:

- **`404.html` in `STATIC_PATH`** — place a `404.html` file at the root of your `STATIC_PATH`. When a file is not found it is served verbatim with a `404` status. No configuration needed, and it takes precedence for 404s.
- **`STATIC_ERROR_PAGE`** — a filesystem path to a custom template used for *all* error statuses (404, 403, 408, 400, 500, …). The template may contain `{{STATUS}}` and `{{REASON}}` placeholders, substituted with the status code and its reason phrase. It is resolved once at startup: if the path is set but the file cannot be read, the server logs an error and exits (it never serves with a misconfigured page). If unset, a built-in page is used.

Internal error detail (e.g. raw storage errors) is never shown to clients — it is logged server-side only.

## Health Check

`GET /health` returns `200 healthy` while the server is running, and `503 unhealthy` after a SIGTERM is received — a drain window (default `5s`, set via `STATIC_SHUTDOWN_DELAY`) that lets the load balancer deregister this instance before shutdown. `503 Service Unavailable` is used (rather than `500`) so balancers read it as a temporary drain and most alerting does not fire.

## CORS

CORS is off until `STATIC_CORS_ALLOW_ORIGIN` is set. It accepts either `*` or a comma-separated allowlist.

```bash
# Allow any origin
STATIC_CORS_ALLOW_ORIGIN=*

# Allowlist — the request Origin is echoed back when it matches, with Vary: Origin
STATIC_CORS_ALLOW_ORIGIN=https://app.example.com,https://admin.example.com

# Preflight tuning
STATIC_CORS_ALLOW_METHODS=GET, HEAD, OPTIONS
STATIC_CORS_ALLOW_HEADERS=Authorization, Content-Type
STATIC_CORS_MAX_AGE=86400

# Credentialed requests — "*" is invalid here, so the matched origin is echoed
STATIC_CORS_ALLOW_CREDENTIALS=true
```

`OPTIONS` preflight requests are answered with `204` and the configured CORS headers before Basic Auth runs (so a credential-less browser preflight is never rejected with `401`). When CORS is disabled, `OPTIONS` and other non-`GET`/`HEAD` methods receive `405 Method Not Allowed`.

## Metrics

When `STATIC_METRICS` is `true` (the default), `GET /metrics` returns Prometheus-format metrics: total requests, responses by status class, total response bytes, in-memory cache hits / misses, a `static_serve_request_duration_seconds` latency **histogram** (time to produce the response), and `static_serve_cache_entries` / `static_serve_cache_capacity` **gauges** for current cache occupancy and configured capacity (their ratio is the fill level). Like `/health`, it bypasses Basic Auth and IP access control — restrict it at the proxy if exposure is a concern, or set `STATIC_METRICS=false` to remove the route entirely. Collection overhead is negligible for normal workloads (a few atomic counters and two clock reads per request); `STATIC_METRICS=false` skips that recording as well, so disabling metrics is genuinely free on the hot path.

## Storage Backends

All backends are powered by [Apache OpenDAL](https://github.com/apache/opendal) and auto-detected from the `STATIC_PATH` format — no extra configuration flags.

### Local Filesystem

The default backend. Set `STATIC_PATH` to a directory path; files are read directly from disk with path-traversal protection.

```bash
STATIC_PATH="/var/www/html"
```

### Amazon S3 (and S3-compatible services)

Activated when `STATIC_PATH` starts with `https://` or `http://`. Works with any S3-compatible service (AWS S3, MinIO, Cloudflare R2, …) by specifying the endpoint URL. Credentials and bucket info are passed as query parameters.

```bash
# AWS S3
STATIC_PATH="https://s3.amazonaws.com?bucket=my-bucket&region=us-east-1&access_key_id=***&secret_access_key=***"

# MinIO / S3-compatible
STATIC_PATH="https://minio.example.com?bucket=static&region=us-east-1&access_key_id=***&secret_access_key=***"
```

| Query Parameter | Description |
|---|---|
| `bucket` | Bucket name (required) |
| `region` | AWS region |
| `access_key_id` | Access key |
| `secret_access_key` | Secret key |

### FTP

Activated when `STATIC_PATH` starts with `ftp://`. Username and password are embedded in the URL.

```bash
STATIC_PATH="ftp://user:password@ftp.example.com/path/to/files"
```

### MongoDB GridFS

Activated when `STATIC_PATH` starts with `mongodb://`. Uses the MongoDB connection string directly, suitable for serving files stored in GridFS.

```bash
STATIC_PATH="mongodb://user:password@mongodb1.example.com:27317,mongodb2.example.com:27017/?connectTimeoutMS=300000&replicaSet=mySet"
```
