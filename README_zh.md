# static

- `STATIC_THREADS`: `tokio` 的 `worker` 线程数，默认为 CPU 核心数。
- `STATIC_PATH`: 静态文件路径，默认为 `/static`。如果 `STATIC_PATH` 以 `https://` 或 `http://` 开头，则认为是 `s3` 服务，其值为 `https://s3.amazonaws.com?bucket=static&region=us-east-1&access_key_id=***&secret_access_key=***`。如果 `STATIC_PATH` 以 `ftp://` 开头，则认为是 `ftp` 服务，其值为 `ftp://user:password@ftp.example.com`。如果 `STATIC_PATH` 以 `mongodb://` 开头，则认为是 `gridfs` 服务，其值为 `mongodb://user:password@mongodb1.example.com:27317,mongodb2.example.com:27017/?connectTimeoutMS=300000&replicaSet=mySet`。
- `STATIC_LISTEN_ADDR`: 监听地址，默认为 `0.0.0.0:3000`。
- `STATIC_TIMEOUT`: 超时时间，默认为 `30s`。
- `STATIC_COMPRESS_MIN_LENGTH`: 启用压缩的最小长度，默认为 `256`。
- `STATIC_INDEX_FILE`: 指定index文件，默认为 `index.html`。
- `STATIC_AUTOINDEX`: 自动索引，默认为 `false`。
- `STATIC_CACHE_CONTROL`: 缓存控制，默认为 `public, max-age=31536000, immutable`，`html` 文件则为 `no-cache`。
- `STATIC_CACHE_SIZE`: 缓存大小，默认为 `1024`。
- `STATIC_CACHE_TTL`: 缓存有效期，默认为 `10m`，`html` 文件不会被缓存。
- `STATIC_HTML_REPLACE_*`: 替换 `html` 内容，例如 `STATIC_HTML_REPLACE_{{HOST}}=https://test.com` 表示将 `{{HOST}}` 替换为 `https://test.com`。
- `STATIC_FALLBACK_INDEX_404`: 当路由返回 `404 Not Found` 时，使用指定的 `index` 文件作为后备。
- `STATIC_FALLBACK_HTML_404`: 当路由返回 `404 Not Found` 时，使用 `.html` 文件作为后备。
- `STATIC_RESPONSE_HEADER_*`: 为所有响应添加自定义 Header，例如 `STATIC_RESPONSE_HEADER_X_FRAME_OPTIONS=DENY` 表示为每个响应添加 `x-frame-options: DENY`。
- `LOG_LEVEL`: 日志级别，默认为 `INFO`，可选值为 `TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR`。


## 健康检查

`GET /health` 在服务正常运行时返回 `200 healthy`。收到 SIGTERM 信号后返回 `500 unhealthy`，并等待 5 秒让负载均衡器完成连接排空后再退出。

## 服务

- `s3`: `STATIC_PATH="https://s3.amazonaws.com?bucket=static&region=us-east-1&access_key_id=***&secret_access_key=***"`
- `ftp`: `STATIC_PATH="ftp://user:password@ftp.example.com"`
- `gridfs`: `STATIC_PATH="mongodb://user:password@mongodb1.example.com:27317,mongodb2.example.com:27017/?connectTimeoutMS=300000&replicaSet=mySet"`
- `fs`: `STATIC_PATH="/static"`


## 用法

```bash
docker run -d --restart=always \
    -p 3000:3000 \
    --name static \
    -v ./static:/static:ro vicanso/static
```