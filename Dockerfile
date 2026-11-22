# syntax=docker/dockerfile:1
# Build and run on the same Alpine release so the musl and library versions match.
ARG ALPINE=3.22

FROM rust:1-alpine${ALPINE} AS build
RUN apk add --no-cache musl-dev alsa-lib-dev sbc-dev pkgconf
# Link alsa-lib and libsbc dynamically: alsa-lib needs its config files at runtime anyway.
ENV RUSTFLAGS="-C target-feature=-crt-static"
ARG APP_VERSION
ENV APP_VERSION=${APP_VERSION}
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs rustfmt.toml ./
COPY src src
COPY web web
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/bluetooth-to-speaker /usr/local/bin/

FROM alpine:${ALPINE}
ARG APP_VERSION=dev
LABEL org.opencontainers.image.title="Bluetooth to Speaker" \
      org.opencontainers.image.description="Turn a Linux board into a Bluetooth speaker with a web UI, DLNA and MQTT" \
      org.opencontainers.image.source="https://github.com/rosenkolev/bluetooth-to-speaker" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${APP_VERSION}"
RUN apk add --no-cache alsa-lib sbc libgcc
COPY --from=build /usr/local/bin/bluetooth-to-speaker /usr/local/bin/bluetooth-to-speaker
ENV DATA_DIR=/data
VOLUME /data
EXPOSE 8780
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD wget -q -O /dev/null "http://127.0.0.1:${HTTP_PORT:-8780}/api/state" || exit 1
ENTRYPOINT ["/usr/local/bin/bluetooth-to-speaker"]
