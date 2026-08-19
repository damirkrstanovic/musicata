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
# Cache the cargo registry + target/ across builds, so a source change recompiles only the
# changed crates instead of the whole dependency tree. target/ is a cache mount (not part of the
# image layer), so copy the binary out to a plain path for the runtime stage to COPY.
# The cache ids are image-specific: this image is bookworm (glibc 2.36) while Dockerfile.ml is
# trixie (glibc 2.41). An unnamed mount's id defaults to its target path, so the two would share
# one target/ cache and each other's build-script binaries wouldn't run under the wrong glibc.
RUN --mount=type=cache,id=musicata-server-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=musicata-server-target,target=/src/target \
    cargo build --release -p musicata-server \
    && cp target/release/musicata-server /musicata-server

# 3. Runtime.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --uid 10001 musicata
COPY --from=build /musicata-server /usr/local/bin/musicata-server
# The AGPL requires the license text to accompany the binary in object form too.
COPY COPYING NOTICE /usr/share/doc/musicata/

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
