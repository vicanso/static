# static

- `STATIC_THREADS`: worker threads for tokio, default is `num cpus`
- `STATIC_PATH`: static file path, default is `/static`, if `STATIC_PATH` starts with `https://` or `http://`, it will be `s3` service, if `STATIC_PATH` starts with `ftp://`, it will be `ftp` service, if `STATIC_PATH` starts with `mongodb://`, it will be `gridfs` service.
- `STATIC_LISTEN_ADDR`: listen address, default is `0.0.0.0:3000`
- `STATIC_TIMEOUT`: timeout, default is `30s`
- `STATIC_COMPRESS_MIN_LENGTH`: compress min length, default is `256`
- `STATIC_INDEX_FILE`: index file, default is `index.html`
- `STATIC_AUTOINDEX`: autoindex, default is `false`
- `STATIC_CACHE_CONTROL`: cache control, default is `public, max-age=31536000, immutable`, and html will be `no-cache`
- `STATIC_CACHE_SIZE`: cache size, default is `1024`
- `STATIC_CACHE_TTL`: cache ttl, default is `10m`, html files will not be cached
- `STATIC_HTML_REPLACE_*`: replace html content, `STATIC_HTML_REPLACE_{{HOST}}=https://test.com` means replace `{{HOST}}` to `https://test.com`
- `STATIC_FALLBACK_INDEX_404`: use index html for 404 not found route
- `STATIC_FALLBACK_HTML_404`: use `.html` for 404 not found route
- `STATIC_RESPONSE_HEADER_*`: add custom response headers, `STATIC_RESPONSE_HEADER_X_FRAME_OPTIONS=DENY` means add `x-frame-options: DENY` to every response
- `LOG_LEVEL`: log level, default is `INFO`, options are `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR`


## Health Check

`GET /health` returns `200 healthy` when the server is running, and `500 unhealthy` after a SIGTERM signal is received (a 5-second window for the load balancer to drain connections).


## Service

- `s3`: `STATIC_PATH="https://s3.amazonaws.com?bucket=static&region=us-east-1&access_key_id=***&secret_access_key=***"`
- `ftp`: `STATIC_PATH="ftp://user:password@ftp.example.com"`
- `gridfs`: `STATIC_PATH="mongodb://user:password@mongodb1.example.com:27317,mongodb2.example.com:27017/?connectTimeoutMS=300000&replicaSet=mySet"`
- `fs`: `STATIC_PATH="/static"`


## Usage

```bash
docker run -d --restart=always \
    -p 3000:3000 \
    --name static \
    -v ./static:/static:ro vicanso/static 
```