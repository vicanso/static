# static

静态文件服务，提供对静态文件的HTTP访问。启动方式如下：

```bash
docker run -it -d --restart=always \
  -v /web:/static:ro \
  -p 3000:3000 \
  vicanso/static
```
