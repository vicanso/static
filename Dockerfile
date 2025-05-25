FROM rust:1.84.0 as builder

COPY . /static


RUN apt update \
    && apt install -y cmake libclang-dev wget gnupg ca-certificates lsb-release --no-install-recommends
RUN rustup target list --installed
RUN cd /static \
    && make release

FROM ubuntu:24.04

EXPOSE 3000 

COPY --from=builder /static/target/release/static-serve /usr/local/bin/static-serve
COPY --from=builder /static/entrypoint.sh /entrypoint.sh

USER ubuntu

CMD ["static-serve"]

ENTRYPOINT ["/entrypoint.sh"]
