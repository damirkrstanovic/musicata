# syntax=docker/dockerfile:1
#
# Self-contained Musicata image (web app embedded in the binary). Build locally:
#
#   docker build -t musicata .
#
# Run with the database + artwork cache on a host directory (mounted at /data) and
# your music library mounted read-only:
#
#   docker run -d --name musicata \
#     -p 3030:3030 \
#     -v /volume1/docker/musicata:/data \
#     -v /volume1/music:/music:ro \
#     musicata
#
# /data must be writable by uid 10001 (the image's `musicata` user); chown it on the
# host, or add `--user "$(id -u):$(id -g)"` to run as yourself.

# 1. Build the embedded web app (arch-independent).
FROM node:20-slim AS web
WORKDIR /web
COPY crates/musicata-server/web/package.json crates/musicata-server/web/package-lock.json ./
RUN npm ci
COPY crates/musicata-server/web/ ./
RUN npm run build

# 2. Build the server. Override the repo's dev-default x86-64-v3 (.cargo/config.toml)
#    with a portable baseline, so the image runs on low-end NAS CPUs (e.g. Celeron J4025).
FROM rust:1-bookworm AS build
ENV RUSTFLAGS="-C target-cpu=x86-64-v2"
WORKDIR /src
COPY . .
COPY --from=web /web/dist crates/musicata-server/web/dist
ENV MUSICATA_SKIP_WEB_BUILD=1
RUN cargo build --release -p musicata-server

# 3. Runtime.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --uid 10001 musicata
COPY --from=build /src/target/release/musicata-server /usr/local/bin/musicata-server

# Database + artwork cache live under /data; mount a host directory there.
ENV MUSICATA_DATABASE=/data/musicata.db
ENV MUSICATA_ADDR=0.0.0.0:3030
ENV MUSICATA_LIBRARY=/music
# Own /data as the runtime user so a fresh (named) volume is writable — Docker seeds a named
# volume from the image's mountpoint ownership. (A host bind-mount still needs chowning on the
# host, or run with `--user`.)
RUN mkdir -p /data && chown 10001:10001 /data
VOLUME /data
EXPOSE 3030
USER musicata
ENTRYPOINT ["musicata-server"]
