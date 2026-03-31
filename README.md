# static

A high-performance static file server built with Rust and [Axum](https://github.com/tokio-rs/axum). It uses [Apache OpenDAL](https://github.com/apache/opendal) as a unified storage abstraction layer, enabling seamless access to multiple storage backends through a single consistent API — no need to switch libraries or rewrite code when changing backends. Supported backends include local filesystem, Amazon S3 (and S3-compatible services), FTP, and MongoDB GridFS.

## Features

- Multiple storage backends via OpenDAL (S3, FTP, GridFS, local filesystem)
- Built-in gzip / brotli / zstd compression
- TinyUFO LRU cache with configurable TTL
- HTTP Range requests (206 Partial Content) for resumable downloads and media seeking
- Optional 304 Not Modified via ETag / If-None-Match
- Directory auto-indexing
- Dynamic HTML content replacement
- Custom response headers
- SPA fallback mode
- Graceful shutdown with SIGTERM drain
- Access logging

## Environment Variables

- `STATIC_THREADS`: worker threads for tokio, default is `num cpus`
- `STATIC_PATH`: static file path, default is `/static`, if `STATIC_PATH` starts with `https://` or `http://`, it will be `s3` service, if `STATIC_PATH` starts with `ftp://`, it will be `ftp` service, if `STATIC_PATH` starts with `mongodb://`, it will be `gridfs` service.
- `STATIC_LISTEN_ADDR`: listen address, default is `0.0.0.0:3000`
- `STATIC_TIMEOUT`: timeout, default is `30s`
- `STATIC_COMPRESS_MIN_LENGTH`: compress min length, default is `256`
- `STATIC_INDEX_FILE`: index file, default is `index.html`
- `STATIC_AUTOINDEX`: autoindex, default is `false`
- `STATIC_CACHE_CONTROL`: cache control, default is `public, max-age=31536000, immutable`, and html will be `no-cache`
- `STATIC_CACHE_SIZE`: cache size, default is `1024`
- `STATIC_CACHE_TTL`: cache ttl, default is `10m`, html files will not be cached
- `STATIC_HTML_REPLACE_*`: replace html content, `STATIC_HTML_REPLACE_{{HOST}}=https://test.com` means replace `{{HOST}}` to `https://test.com`
- `STATIC_FALLBACK_INDEX_404`: use index html for 404 not found route
- `STATIC_FALLBACK_HTML_404`: use `.html` for 404 not found route
- `STATIC_NOT_MODIFIED`: enable `304 Not Modified` support via `If-None-Match` / `ETag`, default is `false`
- `STATIC_PRECOMPRESSED`: enable pre-compressed file support, default is `false`. When enabled, the server checks for `.br` or `.gz` variants of the requested file (e.g., `app.js.br` for `app.js`) and serves them directly if the client supports the encoding, skipping runtime compression
- `STATIC_RESPONSE_HEADER_*`: add custom response headers, `STATIC_RESPONSE_HEADER_X_FRAME_OPTIONS=DENY` means add `x-frame-options: DENY` to every response
- `STATIC_READ_MAX_SIZE`: max file size to buffer in memory, larger files are streamed, supports human-readable format (e.g., `30KB`, `1MB`), default is `250KB`
- `STATIC_ACCESS_LOG`: enable access logging, default is `true`
- `LOG_LEVEL`: log level, default is `INFO`, options are `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR`


## Custom Error Pages

Place a `404.html` file in the root of your `STATIC_PATH` directory. When a file is not found, the server will automatically serve this page with a `404` status code instead of a plain text error. No configuration needed.

## Health Check

`GET /health` returns `200 healthy` when the server is running, and `500 unhealthy` after a SIGTERM signal is received (a 5-second window for the load balancer to drain connections).


## Storage Backends

All storage backends are powered by [Apache OpenDAL](https://github.com/apache/opendal). The backend is automatically detected from the `STATIC_PATH` format — no extra configuration flags needed.

### Local Filesystem

The default backend. Set `STATIC_PATH` to a local directory path. Files are read directly from disk with path traversal protection.

```bash
STATIC_PATH="/var/www/html"
```

### Amazon S3 (and S3-compatible services)

Activated when `STATIC_PATH` starts with `https://` or `http://`. Supports any S3-compatible service (AWS S3, MinIO, Cloudflare R2, etc.) by specifying the endpoint URL. Credentials and bucket info are passed as query parameters.

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


## Usage

```bash
docker run -d --restart=always \
    -p 3000:3000 \
    --name static \
    -v ./static:/static:ro vicanso/static 
```