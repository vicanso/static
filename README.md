# static

- `STATIC_SERVICE`: storage service, default is `fs`, support `fs`, `s3`
- `STATIC_PATH`: static file path, default is `/static`, if `STATIC_SERVICE` is `s3`, it will be `https://s3.amazonaws.com?bucket=static&region=us-east-1&access_key_id=***&secret_access_key=***`
- `STATIC_LISTEN_ADDR`: listen address, default is `127.0.0.1:3000`
- `STATIC_TIMEOUT`: timeout, default is `30s`
- `STATIC_COMPRESS_MIN_LENGTH`: compress min length, default is `256`
- `STATIC_INDEX_FILE`: index file, default is `index.html`
- `STATIC_AUTOINDEX`: autoindex, default is `false`
- `STATIC_CACHE_CONTROL`: cache control, default is `public, max-age=31536000, immutable`, and html will be `no-cache`
- `STATIC_CACHE_SIZE`: cache size, default is `1024`
- `STATIC_CACHE_TTL`: cache ttl, default is `10m`, html files will not be cached
