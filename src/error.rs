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
            Error::Openedal { source } => source.kind() == opendal::ErrorKind::NotFound,
            _ => false,
        }
    }
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
        let status = if is_not_found {
            StatusCode::NOT_FOUND
        } else {
            match self {
                Error::Unknown => StatusCode::INTERNAL_SERVER_ERROR,
                Error::Timeout => StatusCode::REQUEST_TIMEOUT,
                Error::Forbidden => StatusCode::FORBIDDEN,
                _ => StatusCode::BAD_REQUEST,
            }
        };

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
