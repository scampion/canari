# Static musl build: the runtime image carries the binary and a CA bundle,
# nothing else. ring (not aws-lc-rs) is what lets this build with musl-dev
# alone — no cmake, no clang.
FROM rust:1-alpine AS build

RUN apk add --no-cache musl-dev
WORKDIR /src

# Templates, migrations and assets are compiled/embedded into the binary, so
# they must be present at build time.
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY migrations migrations
COPY templates templates
COPY static static

RUN cargo build --release --locked

FROM alpine:3

# rustls verifies peers against the system trust store, so outgoing webhook and
# ntfy deliveries need a CA bundle.
RUN apk add --no-cache ca-certificates \
 && adduser -D -H -u 10001 canari \
 && mkdir -p /data \
 && chown canari:canari /data

COPY --from=build /src/target/release/canari /usr/local/bin/canari

USER canari
VOLUME /data
EXPOSE 8000

ENV CANARI_DB=/data/canari.db \
    CANARI_LISTEN=0.0.0.0:8000

ENTRYPOINT ["canari"]
