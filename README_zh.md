# static

基于 Rust 和 [Axum](https://github.com/tokio-rs/axum) 构建的高性能静态文件服务器。通过 [Apache OpenDAL](https://github.com/apache/opendal) 统一存储抽象层，使用一套一致的 API 无缝对接多种存储后端——切换后端无需更换库或重写代码。支持本地文件系统、Amazon S3（及 S3 兼容服务）、FTP 和 MongoDB GridFS。

## 特性

- 基于 OpenDAL 的多存储后端支持（S3、FTP、GridFS、本地文件系统）
- 内置 gzip / brotli / zstd 压缩
- TinyUFO LRU 缓存，支持可配置的 TTL
- HTTP Range 请求（206 Partial Content），支持断点续传和音视频拖拽播放
- 可选的 304 Not Modified（基于 ETag / If-None-Match）
- 目录自动索引
- HTML 内容动态替换
- 自定义响应头
- SPA 回退模式
- SIGTERM 优雅关闭
- 访问日志

## 环境变量

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
- `STATIC_NOT_MODIFIED`: 启用 `304 Not Modified` 支持，通过 `If-None-Match` / `ETag` 比对实现，默认为 `false`。
- `STATIC_PRECOMPRESSED`: 启用预压缩文件支持，默认为 `false`。启用后，服务器会检查请求文件是否存在 `.br` 或 `.gz` 预压缩副本（如请求 `app.js` 时检查 `app.js.br`），若客户端支持对应编码则直接返回预压缩文件，跳过运行时压缩。
- `STATIC_RESPONSE_HEADER_*`: 为所有响应添加自定义 Header，例如 `STATIC_RESPONSE_HEADER_X_FRAME_OPTIONS=DENY` 表示为每个响应添加 `x-frame-options: DENY`。
- `STATIC_READ_MAX_SIZE`: 直接读入内存的最大文件大小，超过此值的文件将以流式传输，支持可读格式（如 `30KB`、`1MB`），默认为 `250KB`。
- `STATIC_ACCESS_LOG`: 启用访问日志，默认为 `true`。
- `LOG_LEVEL`: 日志级别，默认为 `INFO`，可选值为 `TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR`。


## 自定义错误页

在 `STATIC_PATH` 根目录下放置 `404.html` 文件即可。当请求的文件不存在时，服务器会自动返回该页面并设置 `404` 状态码，无需任何配置。

## 健康检查

`GET /health` 在服务正常运行时返回 `200 healthy`。收到 SIGTERM 信号后返回 `500 unhealthy`，并等待 5 秒让负载均衡器完成连接排空后再退出。

## 存储后端

所有存储后端均基于 [Apache OpenDAL](https://github.com/apache/opendal) 实现。后端类型通过 `STATIC_PATH` 的格式自动识别，无需额外配置。

### 本地文件系统

默认后端。将 `STATIC_PATH` 设为本地目录路径即可，文件直接从磁盘读取，内置路径遍历攻击防护。

```bash
STATIC_PATH="/var/www/html"
```

### Amazon S3（及 S3 兼容服务）

当 `STATIC_PATH` 以 `https://` 或 `http://` 开头时启用。支持任何 S3 兼容服务（AWS S3、MinIO、Cloudflare R2 等），通过 URL 指定端点，凭证和桶信息通过查询参数传递。

```bash
# AWS S3
STATIC_PATH="https://s3.amazonaws.com?bucket=my-bucket&region=us-east-1&access_key_id=***&secret_access_key=***"

# MinIO / S3 兼容服务
STATIC_PATH="https://minio.example.com?bucket=static&region=us-east-1&access_key_id=***&secret_access_key=***"
```

| 查询参数 | 说明 |
|---|---|
| `bucket` | 桶名称（必填） |
| `region` | AWS 区域 |
| `access_key_id` | 访问密钥 |
| `secret_access_key` | 密钥 |

### FTP

当 `STATIC_PATH` 以 `ftp://` 开头时启用。用户名和密码嵌入在 URL 中。

```bash
STATIC_PATH="ftp://user:password@ftp.example.com/path/to/files"
```

### MongoDB GridFS

当 `STATIC_PATH` 以 `mongodb://` 开头时启用。直接使用 MongoDB 连接字符串，适用于从 GridFS 中分发文件。

```bash
STATIC_PATH="mongodb://user:password@mongodb1.example.com:27317,mongodb2.example.com:27017/?connectTimeoutMS=300000&replicaSet=mySet"
```


## 用法

```bash
docker run -d --restart=always \
    -p 3000:3000 \
    --name static \
    -v ./static:/static:ro vicanso/static
```