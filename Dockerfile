FROM rust:1.95.0 as builder

COPY . /static


RUN apt update \
    && apt install -y cmake libclang-dev wget gnupg ca-certificates lsb-release curl --no-install-recommends
RUN rustup target list --installed
RUN curl -L https://github.com/vicanso/http-stat-rs/releases/latest/download/httpstat-linux-musl-$(uname -m).tar.gz | tar -xzf -
    RUN mv httpstat /usr/local/bin/
RUN cd /static \
    && make release

FROM debian:trixie-slim

EXPOSE 3000

# ca-certificates is required by reqwest+rustls (opendal's HTTPS/S3 backend
# panics on Client::new() if the system trust store is empty). trixie-slim
# does not pre-install it.
# Clean apt/dpkg/debconf leftovers and logs in the SAME layer as the install so
# they are never committed: the package lists, apt's .deb cache (incl. its lock
# file), the debconf template caches, dpkg's status-old backup, and all logs.
# (/var/lib/dpkg/status itself is the package DB — keep it.)
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* /var/cache/apt/archives /var/cache/debconf/* /var/lib/dpkg/*-old /var/log/*

# Service account with /bin/false to block login; `-m` still creates a home
# dir so `docker exec -it <container> bash` (invoked explicitly) has a HOME.
RUN useradd -r -m -s /bin/false rust

COPY --from=builder --chown=rust:rust --chmod=755 /static/target/release/static-serve /usr/local/bin/static-serve
COPY --from=builder --chown=rust:rust --chmod=755 /static/entrypoint.sh /entrypoint.sh
COPY --from=builder --chown=rust:rust --chmod=755 /usr/local/bin/httpstat /usr/local/bin/httpstat


USER rust

CMD ["static-serve"]

ENTRYPOINT ["/entrypoint.sh"]
