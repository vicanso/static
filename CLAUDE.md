# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

`static-serve` is a high-performance static file server written in Rust (edition 2024, Axum) aimed at serving frontend static assets. Storage is abstracted through `opendal` (local filesystem, S3, FTP, MongoDB GridFS). Features: TinyUFO LRU cache, gzip/brotli/zstd compression, pre-compressed file support, ETag/304, HTTP range requests, directory auto-indexing, HTML content replacement, Basic Auth, IP allow/block lists, redirects, graceful SIGTERM drain.

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

Five Rust modules (`src/*.rs`) plus compile-time HTML templates. Request flow: `main.rs` fallback route → `serve.rs static_serve()`/`get_file()` → cache check → path validation → opendal `stat` → directory/file handling → header generation → ETag/304 → body (buffered or streamed).

- **`config.rs`** — All `STATIC_*` env vars via `envy`, parsed once in `Config::new()`. **On any parse error it logs and `std::process::exit(1)` — deliberately no silent fallback to defaults.** Duration/byte-size fields (`timeout`, `cache_ttl`, `read_max_size`) are `Option<String>` parsed by hand (`parse_duration_or_exit`/`parse_bytesize_or_exit`). **Do not convert these back to `#[serde(deserialize_with)]`**: combined with the struct's `#[serde(default)]`, `envy 0.4` then fails whenever those vars are absent, which previously made the entire config silently revert to defaults. Prefix-scanned vars (`STATIC_HTML_REPLACE_*`, `STATIC_RESPONSE_HEADER_*`, `STATIC_CACHE_CONTROL_EXT_*`, `STATIC_REDIRECT_*`, `STATIC_BASIC_AUTH_*`) are handled by a manual `std::env::vars()` loop, not the `EnvConfig` struct.
- **`storage.rs`** — `opendal` abstraction; one `Storage` in a `OnceLock`. Backend auto-detected from `STATIC_PATH` (read here, separately from `config.rs`): bare path → local FS; `http(s)://...?bucket=` → S3; `ftp://` → FTP; `mongodb://` → GridFS. Local FS path is canonicalized; `validate()` blocks path traversal (resolved path must stay under root).
- **`serve.rs`** — Core handler. TinyUFO LRU cache; **HTML is never cached** (mutable across deploys via opendal backends). Buffers files `< STATIC_READ_MAX_SIZE` (default 250KB), streams larger ones; supports range/206, 304, per-extension cache-control, and `.br`/`.gz` pre-compressed negotiation. **Pre-compressed responses are intentionally NOT written to the in-memory cache** — the cache key is the logical path and is not encoding-aware, so caching them would serve the wrong encoding to a differently-negotiating client. Don't cache them unless you also make the cache key encoding-aware. The `.wasm`/`.mjs` MIME override only fills in when the type is otherwise undetermined (never clobbers a backend-set Content-Type).
- **`error.rs`** — `Error` enum (`Unknown`, `InvalidFile`, `Timeout`, `NotFound`, `Forbidden`, `Openedal`, `ParseUrl`). `IntoResponse` renders a **built-in HTML error page for every error status**; internal/opendal detail (paths, OS errors) is **never** sent to the client, only logged server-side. Override via `STATIC_ERROR_PAGE` (filesystem path; template uses `{{STATUS}}`/`{{REASON}}`), resolved once at startup by `init_error_template()` (called from `main()`); set-but-unreadable → process exits. Independent from a user `404.html` in `STATIC_PATH` (404-only, served verbatim, takes precedence for 404s).
- **`main.rs`** — Axum router: `GET /health` + catch-all fallback `serve`. Middleware order matters: `HandleErrorLayer` (outermost) → optional `CompressionLayer` (only when `compress_min_length > 0`; `0` disables compression entirely) → timeout → handler. IP block/allow → Basic Auth → redirects are evaluated before file serving. Graceful SIGTERM: `/health` returns 500 for a 5s drain window before shutdown.

**Templates:** `src/templates/error.html` and `src/templates/autoindex.html` are embedded at compile time via `include_str!` (paths relative to `src/error.rs` / `src/serve.rs`). Runtime placeholders: `{{STATUS}}`/`{{REASON}}` (error), `{{CONTENT}}` (autoindex). Edit the `.html` files directly — no Rust change needed.

## Configuration & Non-Obvious Behavior

Full env var reference lives in `README.md` / `README_zh.md`. Key behaviors not obvious from the code:

- HTML always gets `Cache-Control: no-cache` regardless of `STATIC_CACHE_CONTROL`, and is never cached in memory.
- Cache-Control precedence: HTML `no-cache` > backend-provided > `STATIC_CACHE_CONTROL_EXT_<ext>` > `STATIC_CACHE_CONTROL`.
- `STATIC_COMPRESS_MIN_LENGTH=0` removes the compression layer entirely (it does not mean "compress everything").
- `/health` bypasses Basic Auth and IP access control.
- The 408 timeout page is generated outside the compression layer (uncompressed); other error pages are inside it.

## Conventions

- Conventional Commits (`feat:`, `fix:`, `chore:`); history commits directly to `main`.
- `README.md` and `README_zh.md` are a mirrored pair — keep structure and content in sync when changing config or behavior docs.
