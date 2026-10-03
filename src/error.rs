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

use axum::BoxError;
use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use snafu::Snafu;
use std::sync::OnceLock;
use tracing::{error, warn};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("An internal server error occurred"))]
    Unknown,

    #[snafu(display("Invalid file: {message}"))]
    InvalidFile { message: String },

    #[snafu(display("Request timed out"))]
    Timeout,

    #[snafu(display("File not found: {file}"))]
    NotFound { file: String },

    #[snafu(display("Forbidden"))]
    Forbidden,

    #[snafu(display("Moved permanently to {location}"))]
    MovedPermanently { location: String },

    #[snafu(display("Opendal error: {source}"))]
    #[snafu(context(false))]
    Openedal { source: opendal::Error },

    #[snafu(display("Parse url error: {source}"))]
    #[snafu(context(false))]
    ParseUrl { source: url::ParseError },
}
impl Error {
    /// Checks if the error variant represents a "not found" condition.
    pub fn is_not_found(&self) -> bool {
        match self {
            Error::NotFound { .. } => true,
            Error::Openedal { source } => is_opendal_not_found(source),
            _ => false,
        }
    }

    // HTTP status for this error. Backend (opendal) failures are classified by
    // kind: a missing object is 404, a permission problem 403, a transient
    // backend condition (rate limiting, timeouts, connection resets — opendal
    // marks these `temporary`) 503, anything else a genuine 500. None of them
    // is a client error: mapping them to 400 hid backend outages from 5xx
    // alerting entirely.
    fn status(&self) -> StatusCode {
        if self.is_not_found() {
            return StatusCode::NOT_FOUND;
        }
        match self {
            Error::Forbidden => StatusCode::FORBIDDEN,
            Error::InvalidFile { .. } => StatusCode::BAD_REQUEST,
            Error::Timeout => StatusCode::GATEWAY_TIMEOUT,
            Error::Openedal { source } => match source.kind() {
                opendal::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
                opendal::ErrorKind::RateLimited => StatusCode::SERVICE_UNAVAILABLE,
                _ if source.is_temporary() => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            },
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// Whether an opendal error means "this path does not exist". Besides
/// `ErrorKind::NotFound` this covers ENOTDIR: stat'ing `file.txt/x` (a path
/// through a regular file) fails with "not a directory", which opendal folds
/// into `Unexpected` — it is still just a missing path, and must 404 (and let
/// the SPA fallback continue) rather than surface as a server error.
pub fn is_opendal_not_found(e: &opendal::Error) -> bool {
    if e.kind() == opendal::ErrorKind::NotFound {
        return true;
    }
    let mut source = std::error::Error::source(e);
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            return io.kind() == std::io::ErrorKind::NotADirectory;
        }
        source = err.source();
    }
    false
}

pub type Result<T> = std::result::Result<T, Error>;

// Built-in error page, rendered for every error response so clients never
// see raw opendal / internal detail (that is logged server-side instead).
const DEFAULT_ERROR_HTML: &str = include_str!("templates/error.html");

static ERROR_TEMPLATE: OnceLock<String> = OnceLock::new();

/// Resolve the error page once at startup. When `path` is `Some`, the file
/// must be readable; otherwise the process exits — consistent with strict
/// config loading (never serve with a misconfigured custom page silently).
/// When `path` is `None`, the built-in template is used.
pub fn init_error_template(path: Option<&str>) {
    let html = match path {
        Some(p) => std::fs::read_to_string(p).unwrap_or_else(|e| {
            error!("Failed to read STATIC_ERROR_PAGE={p}: {e}");
            std::process::exit(1)
        }),
        None => DEFAULT_ERROR_HTML.to_string(),
    };
    let _ = ERROR_TEMPLATE.set(html);
}

fn error_template() -> &'static str {
    ERROR_TEMPLATE
        .get()
        .map(String::as_str)
        .unwrap_or(DEFAULT_ERROR_HTML)
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        // Normalization redirect (e.g. directory missing its trailing slash):
        // 301 with Location, no body.
        if let Error::MovedPermanently { location } = self {
            let mut resp = StatusCode::MOVED_PERMANENTLY.into_response();
            if let Ok(v) = HeaderValue::try_from(location) {
                resp.headers_mut().insert(header::LOCATION, v);
            }
            return resp;
        }

        let is_not_found = self.is_not_found();
        let status = self.status();

        // Log internal detail server-side; the response body stays generic.
        match &self {
            Error::Openedal { source } if !is_not_found => {
                error!(error = %source, status = status.as_u16(), "opendal error");
            }
            Error::InvalidFile { message } => {
                warn!(detail = %message, "invalid file request");
            }
            Error::Unknown => {
                error!(status = status.as_u16(), "internal error");
            }
            _ => {}
        }

        let reason = status.canonical_reason().unwrap_or("Error");
        let body = error_template()
            .replace("{{STATUS}}", status.as_str())
            .replace("{{REASON}}", reason);

        (
            status,
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            body,
        )
            .into_response()
    }
}

pub async fn handle_error(
    // `Method` and `Uri` are extractors so they can be used here
    method: Method,
    uri: Uri,
    // the last argument must be the error itself
    err: BoxError,
) -> Error {
    if err.is::<tower::timeout::error::Elapsed>() {
        warn!(method = %method, uri = %uri, "request timed out");
        return Error::Timeout;
    }
    error!(
        method = %method,
        uri = %uri,
        error = %err,
        "unhandled internal error",
    );
    // Optimization: Return a generic error to the user, avoiding detail leakage.
    Error::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendal::ErrorKind;

    fn backend(kind: ErrorKind) -> Error {
        Error::Openedal {
            source: opendal::Error::new(kind, "test"),
        }
    }

    #[test]
    fn backend_errors_map_to_server_statuses() {
        assert_eq!(backend(ErrorKind::NotFound).status(), StatusCode::NOT_FOUND);
        assert_eq!(
            backend(ErrorKind::PermissionDenied).status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            backend(ErrorKind::RateLimited).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        // a transient backend failure is 503, a permanent one 500 — never 400
        let transient = Error::Openedal {
            source: opendal::Error::new(ErrorKind::Unexpected, "reset").set_temporary(),
        };
        assert_eq!(transient.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            backend(ErrorKind::Unexpected).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn non_backend_errors_keep_their_statuses() {
        assert_eq!(Error::Timeout.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(Error::Forbidden.status(), StatusCode::FORBIDDEN);
        assert_eq!(Error::Unknown.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let invalid = Error::InvalidFile {
            message: "traversal".to_string(),
        };
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn not_a_directory_is_not_found() {
        // what opendal's fs backend produces for stat("file.txt/x")
        let enotdir = opendal::Error::new(ErrorKind::Unexpected, "not a directory")
            .set_source(std::io::Error::from(std::io::ErrorKind::NotADirectory))
            .set_temporary();
        assert!(is_opendal_not_found(&enotdir));
        let err = Error::Openedal { source: enotdir };
        assert!(err.is_not_found());
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
        // other io errors are not
        let eio = opendal::Error::new(ErrorKind::Unexpected, "io")
            .set_source(std::io::Error::other("disk on fire"));
        assert!(!is_opendal_not_found(&eio));
    }
}
