# syntax=docker/dockerfile:1
#
# Optional musicata-ml image — the audio-embedding service (PANNs CNN14 via ONNX Runtime).
# Kept SEPARATE from the slim server image (Dockerfile): the server never links the ML/ONNX
# stack, and most users won't run ML. Build locally:
#
#   docker build -f Dockerfile.ml -t musicata-ml .
#
# Run it, caching the ~327 MB model on a host directory (mounted at /data) so it's fetched
# once. The server reaches it via the `ml_service_url` setting (e.g. http://musicata-ml:3091):
#
#   docker run -d --name musicata-ml \
#     -p 3091:3091 \
#     -v /volume1/docker/musicata-ml:/data \
#     musicata-ml
#
# /data must be writable by uid 10001 (the image's `musicata` user).

# 1. Build the service. Override the repo's dev-default x86-64-v3 (.cargo/config.toml) with a
#    portable baseline so the image runs on low-end CPUs.
#    NOTE: trixie (glibc 2.41), not bookworm — the prebuilt ONNX Runtime that ort downloads is
#    linked against glibc >= 2.38 (`__isoc23_*` symbols), so it won't link on bookworm (2.36).
FROM rust:1-trixie AS build
ENV RUSTFLAGS="-C target-cpu=x86-64-v2"
# See Dockerfile: `.dockerignore` excludes `.git`, so the revision is passed in by CI.
ARG MUSICATA_GIT_SHA=""
ENV MUSICATA_GIT_SHA=$MUSICATA_GIT_SHA
WORKDIR /src
COPY . .
# Cache the cargo registry + target/ across builds so deps aren't recompiled and ORT isn't
# re-downloaded every time. Both outputs are copied out of the target/ cache mount to plain
# paths (a cache mount isn't part of the image layer the runtime stage COPYs from), and the ORT
# .so stash must run in the same RUN — the mount only exists for the RUN that declares it.
# The cache ids are image-specific: this image is trixie (glibc 2.41) while Dockerfile is bookworm
# (glibc 2.36). An unnamed mount's id defaults to its target path, so the two would share one
# target/ cache and each other's build-script binaries wouldn't run under the wrong glibc.
RUN --mount=type=cache,id=musicata-ml-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=musicata-ml-target,target=/src/target \
    cargo build --release -p musicata-ml \
    && cp target/release/musicata-ml /musicata-ml \
    && mkdir -p /ort && find target/release -name 'libonnxruntime.so*' -exec cp -v {} /ort/ \;

# 2. Runtime. Needs only TLS roots (HTTPS model download) + the ORT lib. Must match the build
#    glibc (trixie), so the downloaded ONNX Runtime resolves at load time.
FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --uid 10001 musicata
COPY --from=build /musicata-ml /usr/local/bin/musicata-ml
COPY --from=build /ort/ /usr/local/lib/
# The AGPL requires the license text to accompany the binary in object form too, and the
# statically linked deps require their own attribution.
COPY COPYING NOTICE THIRD-PARTY-NOTICES.md /usr/share/doc/musicata/
RUN ldconfig
ENV LD_LIBRARY_PATH=/usr/local/lib

# The model (~327 MB) is fetched on first run and cached under /data; mount a host dir there.
ENV MUSICATA_ML_ADDR=0.0.0.0:3091
ENV MUSICATA_ML_MODEL=/data/Cnn14_16k.onnx
ENV MUSICATA_ML_MODEL_URL=https://huggingface.co/pranjal-pravesh/PANNs_CNN14_ONNX/resolve/main/Cnn14_16k.onnx
ENV MUSICATA_ML_DATA_URL=https://huggingface.co/pranjal-pravesh/PANNs_CNN14_ONNX/resolve/main/Cnn14_16k.onnx.data
# Own /data as the runtime user so a fresh (named) volume is writable (Docker seeds a named
# volume from the image's mountpoint ownership).
RUN mkdir -p /data && chown 10001:10001 /data
VOLUME /data
EXPOSE 3091
USER musicata
ENTRYPOINT ["musicata-ml"]
