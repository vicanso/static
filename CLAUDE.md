# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

`static-serve` is a high-performance static file server written in Rust using the Axum web framework. It supports multiple storage backends (local filesystem, S3, FTP, MongoDB GridFS) via the `opendal` abstraction library, with built-in caching, compression, and directory auto-indexing.

## Commands

```bash
make dev       # Run with file watching (requires cargo-watch), STATIC_PATH=~/Downloads
make lint      # Run typos spell checker + cargo clippy --deny=warnings
make release   # Production build (LTO, strip, panic=abort)
cargo test     # Run tests
```

## Architecture

Five modules with clear separation of concerns:

- **`config.rs`** — All configuration loaded from environment variables. Parsed once at startup.
- **`storage.rs`** — Storage abstraction via `opendal`. Initializes a single `Storage` instance (OnceCell). Detects backend from `STATIC_PATH` format (bare path → local FS; `https://...?bucket=...` → S3; `ftp://` → FTP; `mongodb://` → GridFS). Validates paths against traversal attacks.
- **`serve.rs`** — Core file serving handler. Checks TinyUFO LRU cache (HTML files are never cached), stats the file, handles directory listing or index file fallback, detects MIME type, sets ETag/Cache-Control headers, applies HTML content replacement, and streams large files (>30KB) or buffers small ones.
- **`error.rs`** — Custom error types (`Unknown`, `InvalidFile`, `Timeout`, `NotFound`, `Openedal`, `ParseUrl`) that implement `IntoResponse` for HTTP error conversion.
- **`main.rs`** — Sets up Axum router with compression middleware (gzip/brotli/zstd), request timeout, access logging, and graceful SIGTERM shutdown. Two routes: `GET /health` and `GET /*` catch-all.

**Request flow:** `main.rs` route → `serve.rs get_file()` → cache check → path validation → opendal stat → directory/file handling → header generation → ETag check → response body (buffered or streamed).

## Key Configuration (Environment Variables)

| Variable | Default | Purpose |
|---|---|---|
| `STATIC_PATH` | required | Storage path or URL |
| `STATIC_LISTEN_ADDR` | `0.0.0.0:3000` | Bind address |
| `STATIC_THREADS` | num_cpus | Tokio worker threads |
| `STATIC_CACHE_SIZE` | 1024 | LRU cache entries |
| `STATIC_CACHE_TTL` | 10m | Cache TTL |
| `STATIC_CACHE_CONTROL` | `public, max-age=31536000, immutable` | Cache-Control for static assets |
| `STATIC_COMPRESS_MIN_LENGTH` | 256 | Min bytes to compress |
| `STATIC_INDEX_FILE` | `index.html` | Directory index filename |
| `STATIC_AUTOINDEX` | false | Enable directory listing |
| `STATIC_FALLBACK_INDEX_404` | false | Serve index.html for 404s (SPA mode) |
| `STATIC_FALLBACK_HTML_404` | false | Try appending `.html` for 404s |
| `STATIC_HTML_REPLACE_*` | — | Dynamic substitution in HTML responses |
| `STATIC_RESPONSE_HEADER_*` | — | Add custom response headers |

HTML files always get `no-cache` regardless of `STATIC_CACHE_CONTROL`.
