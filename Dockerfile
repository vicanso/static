FROM golang:1.16-alpine as builder

COPY . /static

RUN apk update \
  && apk add git make curl jq \
  && cd /static \
  && make build

FROM alpine 

EXPOSE 7001

# tzdata 安装所有时区配置或可根据需要只添加所需时区

RUN addgroup -g 1000 go \
  && adduser -u 1000 -G go -s /bin/sh -D go \
  && apk add --no-cache tzdata

COPY --from=builder /static/static /usr/local/bin/static
COPY --from=builder /static/entrypoint.sh /entrypoint.sh

USER go

ENV STATIC=/static

WORKDIR /home/go

HEALTHCHECK --timeout=10s --interval=10s CMD [ "wget", "http://127.0.0.1:3000/ping", "-q", "-O", "-"]

CMD ["forest"]

ENTRYPOINT ["/entrypoint.sh"]
