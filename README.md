# static

**English** | [中文](./README_zh.md)

A high-performance static file server built with Rust and [Axum](https://github.com/tokio-rs/axum).

It uses [Apache OpenDAL](https://github.com/apache/opendal) as a unified storage abstraction layer — one consistent API for every backend, so switching storage never means switching libraries or rewriting code. Local filesystem, Amazon S3 (and S3-compatible services), FTP, and MongoDB GridFS all work out of the box.

## Features

- Multiple storage backends via OpenDAL — S3, FTP, GridFS, local filesystem
- Built-in gzip / brotli / zstd compression, plus pre-compressed file support
- TinyUFO LRU cache with configurable size and TTL
- HTTP Range requests (206 Partial Content) for resumable downloads and media seeking
- Optional 304 Not Modified via ETag / If-None-Match
- Directory auto-indexing
- Dynamic HTML content replacement
- Custom response headers
- SPA fallback mode
- Basic Auth and IP allow / block lists
- URL redirect rules
- Graceful shutdown with SIGTERM connection draining
- Access logging

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
| `STATIC_COMPRESS_MIN_LENGTH` | `256` | Minimum response size in bytes to compress |
| `STATIC_PRECOMPRESSED` | `false` | Serve `.br` / `.gz` siblings (e.g. `app.js.br` for `app.js`) when the client supports the encoding, skipping runtime compression |

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

### Content & Headers

| Variable | Default | Description |
|---|---|---|
| `STATIC_HTML_REPLACE_*` | — | Replace HTML content, e.g. `STATIC_HTML_REPLACE_{{HOST}}=https://test.com` replaces `{{HOST}}` with `https://test.com` |
| `STATIC_RESPONSE_HEADER_*` | — | Add a custom response header, e.g. `STATIC_RESPONSE_HEADER_X_FRAME_OPTIONS=DENY` adds `x-frame-options: DENY` to every response |
| `STATIC_ERROR_PAGE` | — | Filesystem path to a custom error page template for all error statuses (uses `{{STATUS}}` / `{{REASON}}`). See [Custom Error Pages](#custom-error-pages). If set but unreadable, the server exits at startup. |

### I/O & Logging

| Variable | Default | Description |
|---|---|---|
| `STATIC_READ_MAX_SIZE` | `250KB` | Max file size buffered in memory; larger files are streamed. Accepts human-readable sizes (`30KB`, `1MB`). |
| `STATIC_ACCESS_LOG` | `true` | Enable access logging |
| `LOG_LEVEL` | `INFO` | Log level: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` |

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

`GET /health` returns `200 healthy` while the server is running, and `500 unhealthy` after a SIGTERM is received — a 5-second window that lets the load balancer drain connections before shutdown.

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
