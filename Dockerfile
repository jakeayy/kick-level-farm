# syntax=docker/dockerfile:1

# ---- build: fully static musl binary ----
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev gcc
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Cache the cargo registry and target dir between builds (BuildKit).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/kick-level-farm /kick-level-farm

# ---- runtime ----
FROM alpine:3
# ca-certificates: TLS roots; tzdata: TZ for log timestamps; su-exec: drop root in the entrypoint.
RUN apk add --no-cache ca-certificates tzdata su-exec

COPY --from=build /kick-level-farm /usr/local/bin/kick-level-farm
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
COPY LICENSE /usr/share/licenses/kick-level-farm/LICENSE

ENV PUID=1000 \
    PGID=1000 \
    TZ=UTC
VOLUME ["/data"]
WORKDIR /data

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
# --wait-for-login: an expired token makes the bot wait for `login` instead of crash-looping.
CMD ["run", "--wait-for-login"]
