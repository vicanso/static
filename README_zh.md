# static

[English](./README.md) | **中文**

基于 Rust 和 [Axum](https://github.com/tokio-rs/axum) 构建的高性能静态文件服务器。

通过 [Apache OpenDAL](https://github.com/apache/opendal) 统一存储抽象层，使用一套一致的 API 对接所有后端——切换存储无需更换库或重写代码。本地文件系统、Amazon S3（及 S3 兼容服务）、FTP、MongoDB GridFS 均开箱即用。

## 特性

- 基于 OpenDAL 的多存储后端——S3、FTP、GridFS、本地文件系统
- 内置 gzip / brotli / zstd 压缩，并支持预压缩文件
- TinyUFO LRU 缓存，大小与 TTL 可配置
- HTTP Range 请求（206 Partial Content），支持断点续传与音视频拖拽
- 可选的 304 Not Modified（基于 ETag / If-None-Match）
- 目录自动索引
- HTML 内容动态替换
- 自定义响应头
- SPA 回退模式
- Basic Auth 与 IP 黑白名单
- URL 重定向规则
- SIGTERM 优雅关闭并排空连接
- 访问日志

## 快速开始

```bash
docker run -d --restart=always \
    -p 3000:3000 \
    --name static \
    -v ./static:/static:ro \
    vicanso/static
```

以上将 `./static` 目录的内容发布在 `3000` 端口。如需开启目录浏览，追加 `-e STATIC_AUTOINDEX=true`。其余行为均通过环境变量配置，详见下文。

## 配置

所有选项均通过环境变量设置，并在启动时一次性解析。

### 核心

| 变量 | 默认值 | 说明 |
|---|---|---|
| `STATIC_PATH` | `/static` | 存储路径或 URL。后端根据 scheme 自动识别——见[存储后端](#存储后端)。 |
| `STATIC_LISTEN_ADDR` | `0.0.0.0:3000` | 监听地址 |
| `STATIC_THREADS` | CPU 核心数 | tokio 工作线程数 |
| `STATIC_TIMEOUT` | `30s` | 请求超时时间 |

### 缓存

| 变量 | 默认值 | 说明 |
|---|---|---|
| `STATIC_CACHE_CONTROL` | `public, max-age=31536000, immutable` | 静态资源的 `Cache-Control`。HTML 始终为 `no-cache`。 |
| `STATIC_CACHE_CONTROL_EXT_*` | — | 按扩展名覆盖，如 `STATIC_CACHE_CONTROL_EXT_WASM=no-cache`。见[按扩展名细化缓存策略](#按扩展名细化缓存策略)。 |
| `STATIC_CACHE_SIZE` | `1024` | LRU 缓存条目数 |
| `STATIC_CACHE_TTL` | `10m` | 缓存有效期。HTML 文件不会被缓存。 |
| `STATIC_NOT_MODIFIED` | `false` | 启用基于 `If-None-Match` / `ETag` 的 `304 Not Modified` |

### 压缩

| 变量 | 默认值 | 说明 |
|---|---|---|
| `STATIC_COMPRESS_MIN_LENGTH` | `256` | 启用压缩的最小响应字节数 |
| `STATIC_PRECOMPRESSED` | `false` | 当客户端支持对应编码时，直接返回 `.br` / `.gz` 副本（如 `app.js` 对应 `app.js.br`），跳过运行时压缩 |

### 路由与回退

| 变量 | 默认值 | 说明 |
|---|---|---|
| `STATIC_INDEX_FILE` | `index.html` | 目录索引文件名 |
| `STATIC_AUTOINDEX` | `false` | 启用目录列表 |
| `STATIC_FALLBACK_INDEX_404` | `false` | 未匹配路由时返回索引文件（SPA 模式） |
| `STATIC_FALLBACK_HTML_404` | `false` | 未匹配路由时尝试追加 `.html` |
| `STATIC_REDIRECT_*` | — | URL 重定向规则。见[重定向规则](#重定向规则)。 |

### 访问控制

| 变量 | 默认值 | 说明 |
|---|---|---|
| `STATIC_BASIC_AUTH_*` | — | Basic Auth 凭据，`STATIC_BASIC_AUTH_<NAME>=user:pass`。见[Basic 认证](#basic-认证)。 |
| `STATIC_BASIC_AUTH_REALM` | `static` | `WWW-Authenticate` 头中的 realm 字符串 |
| `STATIC_IP_ALLOWLIST` | — | 逗号分隔的允许 IP / CIDR。见[IP 访问控制](#ip-访问控制)。 |
| `STATIC_IP_BLOCKLIST` | — | 逗号分隔的阻止 IP / CIDR（优先于白名单检查） |

### 内容与响应头

| 变量 | 默认值 | 说明 |
|---|---|---|
| `STATIC_HTML_REPLACE_*` | — | 替换 HTML 内容，如 `STATIC_HTML_REPLACE_{{HOST}}=https://test.com` 将 `{{HOST}}` 替换为 `https://test.com` |
| `STATIC_RESPONSE_HEADER_*` | — | 添加自定义响应头，如 `STATIC_RESPONSE_HEADER_X_FRAME_OPTIONS=DENY` 为每个响应添加 `x-frame-options: DENY` |
| `STATIC_ERROR_PAGE` | — | 自定义错误页模板的文件系统路径，适用所有错误状态（用 `{{STATUS}}` / `{{REASON}}` 占位符）。见[自定义错误页](#自定义错误页)。设置了但读取失败则启动时退出。 |

### I/O 与日志

| 变量 | 默认值 | 说明 |
|---|---|---|
| `STATIC_READ_MAX_SIZE` | `250KB` | 直接读入内存的最大文件大小，超过则流式传输。支持可读格式（`30KB`、`1MB`）。 |
| `STATIC_ACCESS_LOG` | `true` | 启用访问日志 |
| `LOG_LEVEL` | `INFO` | 日志级别：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` |

## Basic 认证

通过设置一个或多个 `STATIC_BASIC_AUTH_*` 变量为服务器启用 HTTP Basic Auth。`<NAME>` 后缀为任意标识符，用于区分多个账号。

```bash
# 单账号
STATIC_BASIC_AUTH_ADMIN=admin:secret

# 多账号
STATIC_BASIC_AUTH_ALICE=alice:pass1
STATIC_BASIC_AUTH_BOB=bob:pass2

# 自定义浏览器登录弹窗中的 realm（默认：static）
STATIC_BASIC_AUTH_REALM=内部工具
```

设置任意凭据后，未认证的请求将收到带 `WWW-Authenticate` 头的 `401` 响应。`/health` 端点始终绕过 Basic Auth，确保负载均衡器可访问。

## IP 访问控制

通过白名单和黑名单按客户端 IP 限制访问。两者均支持单个 IP 与 CIDR 表示法，兼容 IPv4 和 IPv6。客户端 IP 按 `X-Forwarded-For` → `X-Real-IP` → 连接地址的顺序解析。

```bash
# 屏蔽指定 IP 和网段
STATIC_IP_BLOCKLIST=1.2.3.4,10.0.0.0/8

# 仅允许内网访问
STATIC_IP_ALLOWLIST=192.168.0.0/16,127.0.0.1

# 两者可同时设置——黑名单优先检查
STATIC_IP_BLOCKLIST=1.2.3.4
STATIC_IP_ALLOWLIST=192.168.0.0/16
```

被拒绝的请求收到 `403`。`/health` 端点始终绕过 IP 访问控制。

## 重定向规则

通过 `STATIC_REDIRECT_*` 配置 URL 跳转。`<NAME>` 后缀为任意标识符，用于区分多条规则。

```bash
# 默认 301 永久重定向
STATIC_REDIRECT_HOME=/home /index.html
STATIC_REDIRECT_OLD_DOCS=/docs /v2/docs

# 指定状态码
STATIC_REDIRECT_API=/api 302 https://api.example.com
STATIC_REDIRECT_LEGACY=/old-product 301 https://example.com/new-product
```

格式：`STATIC_REDIRECT_<NAME>=<来源路径> [状态码] <目标>`

- `来源路径` — 精确匹配的请求路径，如 `/old/page`
- `状态码` — 可选，默认为 `301`
- `目标` — 跳转目标，可以是站内路径或完整 URL

重定向在文件服务之前评估，`STATIC_RESPONSE_HEADER_*` 自定义响应头同样会附加到重定向响应中。

## 按扩展名细化缓存策略

默认情况下所有非 HTML 文件均使用 `STATIC_CACHE_CONTROL`。通过 `STATIC_CACHE_CONTROL_EXT_*` 可按扩展名覆盖——扩展名（不含点号）作为变量名后缀，大小写不敏感。

```bash
# WebAssembly 文件禁用缓存（开发阶段频繁更新）
STATIC_CACHE_CONTROL_EXT_WASM=no-cache

# 字体文件缓存一年，不加 immutable（允许重新校验）
STATIC_CACHE_CONTROL_EXT_WOFF2=public, max-age=31536000
STATIC_CACHE_CONTROL_EXT_WOFF=public, max-age=31536000

# JSON 数据文件短缓存
STATIC_CACHE_CONTROL_EXT_JSON=public, max-age=300
```

优先级（从高到低）：

1. HTML 文件 — 固定为 `no-cache`
2. 存储后端返回的 `Cache-Control`
3. `STATIC_CACHE_CONTROL_EXT_<扩展名>` — 按扩展名覆盖
4. `STATIC_CACHE_CONTROL` — 全局默认值

## 自定义错误页

两套相互独立的机制：

- **`STATIC_PATH` 下的 `404.html`** —— 在 `STATIC_PATH` 根目录放置 `404.html`，文件不存在时原样返回该页面并设 `404` 状态码，无需任何配置；对 404 优先生效。
- **`STATIC_ERROR_PAGE`** —— 指向一个自定义模板的文件系统路径，适用于*所有*错误状态（404/403/408/400/500…）。模板可包含 `{{STATUS}}` 和 `{{REASON}}` 占位符，分别替换为状态码与原因短语。启动时解析一次：若设置了路径但文件读取失败，服务器记录错误并退出（绝不带错误配置的页面运行）；未设置时使用内置页面。

内部错误细节（如存储层原始错误）不会暴露给客户端，仅记录在服务端日志。

## 健康检查

`GET /health` 在服务正常运行时返回 `200 healthy`，收到 SIGTERM 信号后返回 `500 unhealthy`——提供 5 秒窗口让负载均衡器在退出前排空连接。

## 存储后端

所有后端均基于 [Apache OpenDAL](https://github.com/apache/opendal) 实现，并根据 `STATIC_PATH` 格式自动识别，无需额外配置标志。

### 本地文件系统

默认后端。将 `STATIC_PATH` 设为目录路径即可，文件直接从磁盘读取，内置路径遍历攻击防护。

```bash
STATIC_PATH="/var/www/html"
```

### Amazon S3（及 S3 兼容服务）

当 `STATIC_PATH` 以 `https://` 或 `http://` 开头时启用。通过指定端点 URL，支持任何 S3 兼容服务（AWS S3、MinIO、Cloudflare R2 等）。凭证和桶信息通过查询参数传递。

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
