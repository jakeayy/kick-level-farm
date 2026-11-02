# syntax=docker/dockerfile:1

# ---- build ----
FROM rust:1-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Cache the cargo registry and target dir between builds (BuildKit).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/kick-level-farm /kick-level-farm

# ---- runtime ----
FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tzdata \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /kick-level-farm /usr/local/bin/kick-level-farm
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh

ENV PUID=1000 \
    PGID=1000 \
    TZ=UTC
VOLUME ["/data"]
WORKDIR /data

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
# --wait-for-login: an expired token makes the bot wait for `login` instead of crash-looping.
CMD ["run", "--wait-for-login"]
