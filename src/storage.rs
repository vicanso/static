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

use crate::error::Error;
#[cfg(any(feature = "s3", feature = "ftp", feature = "gridfs"))]
use opendal::Builder;
use opendal::{Operator, layers::MimeGuessLayer};
use path_absolutize::Absolutize;
#[cfg(any(feature = "s3", feature = "ftp"))]
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use tracing::{error, info};
#[cfg(any(feature = "s3", feature = "ftp"))]
use url::Url;

type Result<T> = std::result::Result<T, Error>;
static STORAGE: OnceLock<Storage> = OnceLock::new();
static SKIP_SYMLINK_CHECK: OnceLock<bool> = OnceLock::new();

// Whether to skip the per-request symlink-escape `canonicalize()` syscall in
// `validate`. Read once from STATIC_DISABLE_SYMLINK_CHECK. The lexical `..`
// traversal block stays on regardless; this only drops the FS round-trip that
// catches symlinks pointing outside the root. Parsed strictly (exit on a bad
// value) to match the rest of the config's no-silent-fallback policy.
fn skip_symlink_check() -> bool {
    *SKIP_SYMLINK_CHECK.get_or_init(|| {
        match std::env::var("STATIC_DISABLE_SYMLINK_CHECK").as_deref() {
            Ok("true") => true,
            Ok("false") | Err(_) => false,
            Ok(other) => {
                error!("Invalid STATIC_DISABLE_SYMLINK_CHECK={other}: expected true or false");
                std::process::exit(1)
            }
        }
    })
}

pub struct Storage {
    pub dal: Operator,
    root: Option<PathBuf>,
}

impl Storage {
    pub fn validate(&self, file: &str) -> Result<()> {
        if let Some(root_path) = &self.root {
            let full_path = root_path.join(file);

            let validated_path = full_path.absolutize().map_err(|e| Error::InvalidFile {
                message: e.to_string(),
            })?;
            if !validated_path.starts_with(root_path) {
                return Err(Error::InvalidFile {
                    message: format!("Path traversal attempt blocked, file: {file}"),
                });
            }
            // `absolutize` is purely lexical. Harden against symlinks that
            // escape the (already-canonical) root: if the target exists, its
            // canonical path must also stay under the root. This is the only FS
            // syscall here; STATIC_DISABLE_SYMLINK_CHECK=true skips it for asset
            // trees known to be symlink-free (the lexical check above stays on).
            if !skip_symlink_check()
                && let Ok(canonical) = full_path.canonicalize()
                && !canonical.starts_with(root_path)
            {
                return Err(Error::InvalidFile {
                    message: format!("Path escapes root via symlink, file: {file}"),
                });
            }
        }
        Ok(())
    }
}

#[cfg(any(feature = "s3", feature = "ftp"))]
struct StorageParams {
    user: String,
    password: Option<String>,
    endpoint: String,
    path: String,
    query: HashMap<String, String>,
}

#[cfg(any(feature = "s3", feature = "ftp"))]
fn parse_params(url: &str) -> Result<StorageParams> {
    let info = Url::parse(url).map_err(|e| Error::ParseUrl { source: e })?;
    let port_str = info.port().map(|p| format!(":{p}")).unwrap_or_default();
    let endpoint = format!(
        "{}://{}{}",
        info.scheme(),
        info.host_str().unwrap_or_default(),
        port_str
    );

    let query = info
        .query_pairs()
        .into_owned()
        .collect::<HashMap<String, String>>();

    Ok(StorageParams {
        user: info.username().to_string(),
        password: info.password().map(|v| v.to_string()),
        endpoint,
        path: info.path().to_string(),
        query,
    })
}

#[cfg(any(feature = "s3", feature = "ftp", feature = "gridfs"))]
fn build_operator<B: Builder>(builder: B) -> Result<Operator> {
    let dal = Operator::new(builder)
        .map_err(|e| Error::Openedal { source: e })?
        .layer(MimeGuessLayer::default())
        .finish();
    Ok(dal)
}

#[cfg(feature = "s3")]
fn new_s3_dal(url: &str) -> Result<Storage> {
    let params = parse_params(url)?;
    let mut builder = opendal::services::S3::default().endpoint(&params.endpoint);
    if !params.path.is_empty() {
        builder = builder.root(&params.path);
    }
    if let Some(bucket) = params.query.get("bucket") {
        builder = builder.bucket(bucket);
    }
    if let Some(region) = params.query.get("region") {
        builder = builder.region(region);
    }
    if let Some(access_key_id) = params.query.get("access_key_id") {
        builder = builder.access_key_id(access_key_id);
    }
    if let Some(secret_access_key) = params.query.get("secret_access_key") {
        builder = builder.secret_access_key(secret_access_key);
    }

    info!(
        category = "s3",
        endpoint = params.endpoint,
        "initialize storage"
    );
    Ok(Storage {
        dal: build_operator(builder)?,
        root: None,
    })
}

#[cfg(not(feature = "s3"))]
fn new_s3_dal(_url: &str) -> Result<Storage> {
    Err(Error::InvalidFile {
        message: "s3 backend is not enabled in this build (rebuild with --features s3)".to_string(),
    })
}

#[cfg(feature = "ftp")]
fn new_ftp_dal(url: &str) -> Result<Storage> {
    let params = parse_params(url)?;
    let mut builder = opendal::services::Ftp::default().endpoint(&params.endpoint);
    if !params.path.is_empty() {
        builder = builder.root(&params.path);
    }
    if !params.user.is_empty() {
        builder = builder.user(&params.user);
    }
    if let Some(password) = params.password {
        builder = builder.password(&password);
    }
    info!(
        category = "ftp",
        endpoint = params.endpoint,
        "initialize storage"
    );
    Ok(Storage {
        dal: build_operator(builder)?,
        root: None,
    })
}

#[cfg(not(feature = "ftp"))]
fn new_ftp_dal(_url: &str) -> Result<Storage> {
    Err(Error::InvalidFile {
        message: "ftp backend is not enabled in this build (rebuild with --features ftp)"
            .to_string(),
    })
}

#[cfg(feature = "gridfs")]
fn new_gridfs_dal(url: &str) -> Result<Storage> {
    let builder = opendal::services::Gridfs::default().connection_string(url);
    info!(category = "gridfs", "initialize storage");
    Ok(Storage {
        dal: build_operator(builder)?,
        root: None,
    })
}

#[cfg(not(feature = "gridfs"))]
fn new_gridfs_dal(_url: &str) -> Result<Storage> {
    Err(Error::InvalidFile {
        message: "gridfs backend is not enabled in this build (rebuild with --features gridfs)"
            .to_string(),
    })
}

pub fn get_storage() -> Result<&'static Storage> {
    if let Some(storage) = STORAGE.get() {
        return Ok(storage);
    }
    let storage = {
        let static_path = std::env::var("STATIC_PATH").unwrap_or_else(|_| "/static".to_string());

        match static_path {
            static_path
                if static_path.starts_with("https://") || static_path.starts_with("http://") =>
            {
                new_s3_dal(&static_path)
            }
            static_path if static_path.starts_with("ftp://") => new_ftp_dal(&static_path),
            static_path if static_path.starts_with("mongodb://") => new_gridfs_dal(&static_path),
            _ => {
                let abs_path = PathBuf::from(&static_path)
                    .canonicalize()
                    .unwrap_or_else(|_| PathBuf::from(&static_path));
                let opendal = opendal::services::Fs::default()
                    .root(abs_path.to_str().unwrap_or(&static_path));
                info!(category = "fs", path = %abs_path.to_string_lossy(), "initialize storage");
                let dal = opendal::Operator::new(opendal)
                    .map_err(|e| Error::Openedal { source: e })?
                    .layer(MimeGuessLayer::default())
                    .finish();
                Ok(Storage {
                    dal,
                    root: Some(abs_path),
                })
            }
        }
    }?;
    Ok(STORAGE.get_or_init(|| storage))
}
