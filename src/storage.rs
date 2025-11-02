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

use crate::error::Error;
use once_cell::sync::OnceCell;
use opendal::{Operator, layers::MimeGuessLayer};
use std::collections::HashMap;
use url::Url;

type Result<T> = std::result::Result<T, Error>;

pub struct Storage {
    pub dal: Operator,
}

static STORAGE: OnceCell<Storage> = OnceCell::new();

struct StorageParams {
    user: String,
    password: Option<String>,
    endpoint: String,
    path: String,
    query: HashMap<String, String>,
}

fn parse_params(url: &str) -> Result<StorageParams> {
    let info = Url::parse(url).map_err(|e| Error::ParseUrl { source: e })?;
    let mut endpoint = format!(
        "{}://{}",
        info.scheme(),
        info.host().map(|v| v.to_string()).unwrap_or_default()
    );
    if let Some(port) = info.port() {
        endpoint = format!("{endpoint}:{port}");
    }

    let mut query = HashMap::new();
    info.query_pairs().for_each(|(k, v)| {
        query.insert(k.to_string(), v.to_string());
    });

    Ok(StorageParams {
        user: info.username().to_string(),
        password: info.password().map(|v| v.to_string()),
        endpoint,
        path: info.path().to_string(),
        query,
    })
}

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

    let dal = opendal::Operator::new(builder)
        .map_err(|e| Error::Openedal { source: e })?
        .layer(MimeGuessLayer::default())
        .finish();
    Ok(Storage { dal })
}

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
    let dal = opendal::Operator::new(builder)
        .map_err(|e| Error::Openedal { source: e })?
        .layer(MimeGuessLayer::default())
        .finish();
    Ok(Storage { dal })
}

fn new_gridfs_dal(url: &str) -> Result<Storage> {
    let builder = opendal::services::Gridfs::default().connection_string(url);
    let dal = opendal::Operator::new(builder)
        .map_err(|e| Error::Openedal { source: e })?
        .layer(MimeGuessLayer::default())
        .finish();
    Ok(Storage { dal })
}

pub fn get_storage() -> Result<&'static Storage> {
    let storage = STORAGE.get_or_try_init(|| {
        let static_path = std::env::var("STATIC_PATH").unwrap_or("/static".to_string());

        match static_path {
            static_path
                if static_path.starts_with("https://") || static_path.starts_with("http://") =>
            {
                new_s3_dal(&static_path)
            }
            static_path if static_path.starts_with("ftp://") => new_ftp_dal(&static_path),
            static_path if static_path.starts_with("mongodb://") => new_gridfs_dal(&static_path),
            _ => {
                let opendal = opendal::services::Fs::default().root(static_path.as_str());
                let dal = opendal::Operator::new(opendal)
                    .map_err(|e| Error::Openedal { source: e })?
                    .layer(MimeGuessLayer::default())
                    .finish();
                Ok(Storage { dal })
            }
        }
    })?;
    Ok(storage)
}
