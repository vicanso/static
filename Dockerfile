FROM rust:1.95.0 as builder

COPY . /static


RUN apt update \
    && apt install -y cmake libclang-dev wget gnupg ca-certificates lsb-release curl --no-install-recommends
RUN rustup target list --installed
RUN curl -L https://github.com/vicanso/http-stat-rs/releases/latest/download/httpstat-linux-musl-$(uname -m).tar.gz | tar -xzf -
    RUN mv httpstat /usr/local/bin/
RUN cd /static \
    && make release

FROM debian:bookworm-slim

EXPOSE 3000

RUN useradd -r -s /bin/false ubuntu

COPY --from=builder --chown=ubuntu:ubuntu --chmod=755 /static/target/release/static-serve /usr/local/bin/static-serve
COPY --from=builder --chown=ubuntu:ubuntu --chmod=755 /static/entrypoint.sh /entrypoint.sh
COPY --from=builder --chown=ubuntu:ubuntu --chmod=755 /usr/local/bin/httpstat /usr/local/bin/httpstat


USER ubuntu

CMD ["static-serve"]

ENTRYPOINT ["/entrypoint.sh"]
