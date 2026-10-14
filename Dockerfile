FROM rust:1.88.0 AS builder

WORKDIR /app

COPY . .

ARG BUILD_PROFILE=dev
RUN case "$BUILD_PROFILE" in \
        dev) cargo build --locked --package turso_cli && cp target/debug/tursodb /tmp/tursodb ;; \
        dist) cargo build --locked --package turso_cli --profile dist && cp target/dist/tursodb /tmp/tursodb ;; \
        *) exit 1 ;; \
    esac

FROM rust:1.88.0-slim

RUN mkdir /data && chown 10001:10001 /data
WORKDIR /data
ENV HOME=/data

COPY --from=builder /tmp/tursodb /usr/local/bin/tursodb
USER 10001:10001

EXPOSE 8080
STOPSIGNAL SIGINT
ENTRYPOINT ["tursodb"]
CMD ["--sync-server", "0.0.0.0:8080", "/data/turso.db"]
