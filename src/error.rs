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

use axum::BoxError;
use axum::http::HeaderValue;
use axum::http::{Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use snafu::Snafu;
use tracing::error;

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
    #[snafu(display("Openedal error: {source}"))]
    Openedal { source: opendal::Error },
    #[snafu(display("Parse url error: {source}"))]
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

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, message) = if self.is_not_found() {
            (StatusCode::NOT_FOUND, self.to_string())
        } else {
            match self {
                Error::Unknown => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
                Error::Timeout => (StatusCode::REQUEST_TIMEOUT, self.to_string()),
                _ => (StatusCode::BAD_REQUEST, self.to_string()),
            }
        };
        let mut res = message.into_response();
        res.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        (status, res).into_response()
    }
}

pub async fn handle_error(
    // `Method` and `Uri` are extractors so they can be used here
    method: Method,
    uri: Uri,
    // the last argument must be the error itself
    err: BoxError,
) -> Error {
    error!(
        method = %method,
        uri = %uri,
        error = %err,
        "unhandled internal error",
    );
    if err.is::<tower::timeout::error::Elapsed>() {
        return Error::Timeout;
    }
    // Optimization: Return a generic error to the user, avoiding detail leakage.
    Error::Unknown
}
