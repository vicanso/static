FROM rust:1.88.0 as builder

COPY . /static


RUN apt update \
    && apt install -y cmake libclang-dev wget gnupg ca-certificates lsb-release curl --no-install-recommends
RUN rustup target list --installed
RUN curl -L https://github.com/vicanso/http-stat-rs/releases/latest/download/httpstat-linux-musl-$(uname -m).tar.gz | tar -xzf -
    RUN mv httpstat /usr/local/bin/
RUN cd /static \
    && make release

FROM ubuntu:24.04

EXPOSE 3000 

COPY --from=builder /static/target/release/static-serve /usr/local/bin/static-serve
COPY --from=builder /static/entrypoint.sh /entrypoint.sh
COPY --from=builder /usr/local/bin/httpstat /usr/local/bin/httpstat


USER ubuntu

CMD ["static-serve"]

ENTRYPOINT ["/entrypoint.sh"]
