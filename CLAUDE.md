# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

`static-serve` is a high-performance static file server written in Rust (edition 2024, Axum) aimed at serving frontend static assets. Storage is abstracted through `opendal` (local filesystem, S3, FTP, MongoDB GridFS). Features: TinyUFO LRU cache, gzip/brotli/zstd compression, pre-compressed file support, ETag/304, HTTP range requests + If-Range, directory auto-indexing, HTML content replacement, CORS, Basic Auth, IP allow/block lists, redirects, nosniff default, Prometheus `/metrics`, text/JSON access logs, configurable graceful SIGTERM drain.

## Commands

```bash
make dev      # cargo watch, STATIC_PATH=./assets, RUST_LOG=INFO  (needs cargo-watch)
make lint     # typos + cargo clippy --all-targets --all -- --deny=warnings
make fmt      # cargo fmt
make release  # cargo build --release  (LTO/strip/panic=abort come from Cargo.toml [profile.release])
make bloat    # cargo bloat --release
```

There is currently **no test suite** (no `#[test]`, no `tests/`); `cargo test` is a no-op. Once tests exist, run one with `cargo test <name>`.

`make lint` is the gate that matters and must pass. Beyond `--deny=warnings`, `Cargo.toml` sets `[lints.clippy] unwrap_used = "deny"` — **do not use `.unwrap()`**; propagate via the `Error` type / `Result`. Run `cargo clippy` (or `make lint`) before considering any change done.

## Architecture

Six Rust modules (`src/*.rs`) plus compile-time HTML templates. Request flow: `main.rs` fallback route → `serve.rs static_serve()`/`get_file()` → cache check → path validation → opendal `stat` → directory/file handling → header generation → ETag/304 → body (buffered or streamed).

- **`config.rs`** — All `STATIC_*` env vars via `envy`, parsed once in `Config::new()`. **On any parse error it logs and `std::process::exit(1)` — deliberately no silent fallback to defaults.** Duration/byte-size fields (`timeout`, `cache_ttl`, `read_max_size`, `shutdown_delay`) are `Option<String>` parsed by hand (`parse_duration_or_exit`/`parse_bytesize_or_exit`); `STATIC_CORS_MAX_AGE` is likewise validated (digits only) with exit-on-bad-value. **Do not convert these back to `#[serde(deserialize_with)]`**: combined with the struct's `#[serde(default)]`, `envy 0.4` then fails whenever those vars are absent, which previously made the entire config silently revert to defaults. Prefix-scanned vars (`STATIC_HTML_REPLACE_*`, `STATIC_RESPONSE_HEADER_*`, `STATIC_CACHE_CONTROL_EXT_*`, `STATIC_REDIRECT_*`, `STATIC_BASIC_AUTH_*`) are handled by a manual `std::env::vars()` loop, not the `EnvConfig` struct.
- **`storage.rs`** — `opendal` abstraction; one `Storage` in a `OnceLock`. Backend auto-detected from `STATIC_PATH` (read here, separately from `config.rs`): bare path → local FS; `http(s)://...?bucket=` → S3; `ftp://` → FTP; `mongodb://` → GridFS. Local FS path is canonicalized; `validate()` blocks path traversal (resolved path must stay under root).
- **`serve.rs`** — Core handler. TinyUFO LRU cache; **HTML and directory autoindex listings are never cached** (mutable across deploys/requests via opendal backends). Buffers files `< STATIC_READ_MAX_SIZE` (default 250KB), streams larger ones; supports range/206, `If-Range`, 304, per-extension cache-control, and `.br`/`.zst`/`.gz` pre-compressed negotiation (q-value aware via `encoding_accepted` — `br;q=0` is honored as a refusal). **Pre-compressed responses are intentionally NOT written to the in-memory cache** — the cache key is the logical path and is not encoding-aware, so caching them would serve the wrong encoding to a differently-negotiating client. Don't cache them unless you also make the cache key encoding-aware. File responses always carry `Vary: Accept-Encoding` (the global CompressionLayer may compress on the fly regardless of `STATIC_PRECOMPRESSED`). `If-Range` uses strong validators only — a weak ETag never satisfies it (RFC 7233), so the Range is ignored and a full 200 is returned. Autoindex file names are HTML-escaped and percent-encoded (`html_escape`) — they are attacker-influenced. The `.wasm`/`.mjs` MIME override only fills in when the type is otherwise undetermined (never clobbers a backend-set Content-Type). Cache hits/misses are counted via `crate::metrics`.
- **`error.rs`** — `Error` enum (`Unknown`, `InvalidFile`, `Timeout`, `NotFound`, `Forbidden`, `Openedal`, `ParseUrl`). `IntoResponse` renders a **built-in HTML error page for every error status**; internal/opendal detail (paths, OS errors) is **never** sent to the client, only logged server-side. Override via `STATIC_ERROR_PAGE` (filesystem path; template uses `{{STATUS}}`/`{{REASON}}`), resolved once at startup by `init_error_template()` (called from `main()`); set-but-unreadable → process exits. Independent from a user `404.html` in `STATIC_PATH` (404-only, served verbatim, takes precedence for 404s).
- **`metrics.rs`** — Process-memory `AtomicU64` counters (requests, responses by status class, response bytes, cache hits/misses) rendered as Prometheus text. Exposed at `GET /metrics` when `STATIC_METRICS=true` (default). No external deps; `Relaxed` ordering (scrape metrics need no cross-counter consistency).
- **`main.rs`** — Axum router: `GET /health`, optional `GET /metrics`, catch-all `fallback(serve)` (a bare handler, **not** `get(serve)`, so `OPTIONS` reaches the handler for CORS preflight; other non-GET/HEAD methods get 405). Middleware order matters (outermost→in): `track_metrics` (records sample, strips the internal `x-original-size` header so it never leaks to clients) → optional `access_log` → `HandleErrorLayer` → optional `CompressionLayer` (only when `compress_min_length > 0`; `0` disables compression entirely) → timeout → handler. In `serve`: IP block/allow → **CORS preflight / method gate (before Basic Auth, so a credential-less browser preflight is not 401'd)** → Basic Auth → redirects → file serving. `apply_common_headers` adds `STATIC_RESPONSE_HEADER_*`, the `X-Content-Type-Options: nosniff` default (skipped if you set it yourself), and CORS headers to every served/redirect/404 response. Graceful SIGTERM: `/health` returns 500 for a `STATIC_SHUTDOWN_DELAY` drain window (default 5s) before shutdown. `LOG_FORMAT=json` switches the tracing subscriber to JSON.

**Templates:** `src/templates/error.html` and `src/templates/autoindex.html` are embedded at compile time via `include_str!` (paths relative to `src/error.rs` / `src/serve.rs`). Runtime placeholders: `{{STATUS}}`/`{{REASON}}` (error), `{{CONTENT}}` (autoindex). Edit the `.html` files directly — no Rust change needed.

## Configuration & Non-Obvious Behavior

Full env var reference lives in `README.md` / `README_zh.md`. Key behaviors not obvious from the code:

- HTML always gets `Cache-Control: no-cache` regardless of `STATIC_CACHE_CONTROL`, and is never cached in memory.
- Cache-Control precedence: HTML `no-cache` > backend-provided > `STATIC_CACHE_CONTROL_EXT_<ext>` > `STATIC_CACHE_CONTROL`.
- `STATIC_COMPRESS_MIN_LENGTH=0` removes the compression layer entirely (it does not mean "compress everything").
- `/health` and `/metrics` are separate routes — they bypass Basic Auth and IP access control. `/metrics` is removable with `STATIC_METRICS=false`.
- CORS preflight (`OPTIONS`) is answered before Basic Auth so a credential-less browser preflight is never 401'd; it still passes IP access control.
- `X-Content-Type-Options: nosniff` is added by default (`STATIC_CONTENT_TYPE_NOSNIFF=true`) but never overrides a value you set via `STATIC_RESPONSE_HEADER_*`.
- File responses always carry `Vary: Accept-Encoding`; CORS allowlist matches append `Vary: Origin`. Autoindex listings are `no-cache` and never stored in the in-memory cache.
- The 408 timeout page is generated outside the compression layer (uncompressed); other error pages are inside it. Error responses do not pass through `apply_common_headers`, so they carry no CORS/nosniff headers.

## Conventions

- Conventional Commits (`feat:`, `fix:`, `chore:`); history commits directly to `main`.
- `README.md` and `README_zh.md` are a mirrored pair — keep structure and content in sync when changing config or behavior docs.
