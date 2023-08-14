# static

静态文件服务，提供对静态文件的HTTP访问。启动方式如下：

```bash
docker run -d --restart=always \
  -v /web:/static:ro \
  -p 3000:3000 \
  vicanso/static
```

## 环境变量

应用支持通过以下环境变量：

- `STATIC_PATH`: 静态文件目录，默认为`/static`
- `STATIC_COMPRESS_LEVEL`: 静态文件压缩级别，默认为`6`
- `STATIC_COMPRESS_MIN_LENGTH`: 最小压缩长度，默认为`1024`，只压缩大于等于1KB的文件
- `STATIC_COMPRESS_CONTENT_TYPE`: 压缩的文件类型，使用正则判断，默认为`text|javascript|json|wasm|font`
- `STATIC_CACHE_TTL`: 缓存文件有效期，如果不设置则为`10m`
- `STATIC_DISABLE_LOG`: 是否禁用访问日志
