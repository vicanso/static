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

use axum::http::HeaderValue;
use axum::http::{header, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::BoxError;
use snafu::Snafu;
use tracing::error;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("IO error: {file}, {source}"))]
    Io {
        file: String,
        source: std::io::Error,
    },
    #[snafu(display("Unknown error: {message}"))]
    Unknown {
        message: String,
    },
    #[snafu(display("Pattern error: {source}"))]
    Pattern {
        source: glob::PatternError,
    },
    #[snafu(display("Glob error: {source}"))]
    Glob {
        source: glob::GlobError,
    },
    Timeout,
    #[snafu(display("File not found: {file}"))]
    NotFound {
        file: String,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        // let status = match StatusCode::from_u16(self.status) {
        //     Ok(status) => status,
        //     Err(_) => StatusCode::BAD_REQUEST,
        // };
        // 对于出错设置为no-cache
        let mut res = self.to_string().into_response();
        res.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        (StatusCode::BAD_REQUEST, res).into_response()
    }
}

pub async fn handle_error(
    // `Method` and `Uri` are extractors so they can be used here
    method: Method,
    uri: Uri,
    // the last argument must be the error itself
    err: BoxError,
) -> Error {
    error!("method:{}, uri:{}, error:{}", method, uri, err.to_string());
    if err.is::<tower::timeout::error::Elapsed>() {
        return Error::Timeout;
    }
    Error::Unknown {
        message: err.to_string(),
    }
    // HttpError::new(&err.to_string())
}
