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
use url::Url;

pub struct Storage {
    pub dal: opendal::Operator,
}

static STORAGE: OnceCell<Storage> = OnceCell::new();

fn new_s3_dal(url: &str) -> Result<Storage, Error> {
    let info = Url::parse(url).map_err(|e| Error::ParseUrl { source: e })?;
    let mut builder = opendal::services::S3::default();
    if let Some(host) = info.host() {
        builder = builder.endpoint(&format!("{}://{}", info.scheme(), host));
    }
    let path = info.path();
    if !path.is_empty() {
        builder = builder.root(path);
    }
    let mut bucket = "".to_string();
    let mut region = "".to_string();
    let mut access_key_id = "".to_string();
    let mut secret_access_key = "".to_string();
    info.query_pairs().for_each(|(k, v)| {
        match k.to_string().as_str() {
            "bucket" => {
                bucket = v.to_string();
            }
            "region" => {
                region = v.to_string();
            }
            "access_key_id" => {
                access_key_id = v.to_string();
            }
            "secret_access_key" => {
                secret_access_key = v.to_string();
            }
            _ => {}
        };
    });
    if !bucket.is_empty() {
        builder = builder.bucket(&bucket);
    }
    if !region.is_empty() {
        builder = builder.region(&region);
    }
    if !access_key_id.is_empty() {
        builder = builder.access_key_id(&access_key_id);
    }
    if !secret_access_key.is_empty() {
        builder = builder.secret_access_key(&secret_access_key);
    }

    let dal = opendal::Operator::new(builder)
        .map_err(|e| Error::Openedal { source: e })?
        .finish();
    Ok(Storage { dal })
}

pub fn get_storage() -> Result<&'static Storage, Error> {
    let storage = STORAGE.get_or_try_init(|| {
        let static_service = std::env::var("STATIC_SERVICE").unwrap_or_default();
        let static_path = std::env::var("STATIC_PATH").unwrap_or("/static".to_string());
        match static_service.as_str() {
            "s3" => new_s3_dal(&static_path),
            _ => {
                let opendal = opendal::services::Fs::default().root(static_path.as_str());
                let dal = opendal::Operator::new(opendal)
                    .map_err(|e| Error::Openedal { source: e })?
                    .finish();
                Ok(Storage { dal })
            }
        }
    })?;
    Ok(storage)
}
