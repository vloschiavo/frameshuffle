# syntax=docker/dockerfile:1
#
# HEIC/HEIF support is OPT-IN:   docker build --build-arg WITH_HEIC=1 -t frameshuffle:heic .
#
# HEIC images are HEVC video frames, and HEVC decoders (libde265) may be covered by patents in some
# jurisdictions. The official published image therefore contains no HEIC decoder (WITH_HEIC=0, the
# default). Build the variant yourself if you want it, or convert your photos once with
# scripts/convert-heic.sh. See "HEIC photos" in the README.
#
# Ubuntu 24.04 for both stages so the libheif the binary links against at build time is the one
# present at runtime (1.17.x, which also ships the libde265 plugin HEIC needs).

FROM ubuntu:24.04 AS build
ARG WITH_HEIC=0
ENV DEBIAN_FRONTEND=noninteractive
RUN set -eux; \
    pkgs="rustc-1.91 cargo-1.91 build-essential pkg-config ca-certificates"; \
    if [ "$WITH_HEIC" = "1" ]; then pkgs="$pkgs zlib1g-dev libheif-dev libheif-plugin-libde265"; fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends $pkgs; \
    rm -rf /var/lib/apt/lists/*
ENV PATH=/usr/lib/rust-1.91/bin:$PATH
WORKDIR /src

# Dependency layer: only re-runs when Cargo.toml/Cargo.lock (or WITH_HEIC) change.
COPY Cargo.toml Cargo.lock ./
RUN set -eux; \
    features=""; if [ "$WITH_HEIC" != "1" ]; then features="--no-default-features"; fi; \
    mkdir src && echo 'fn main(){}' > src/main.rs; \
    cargo build --release --locked $features; \
    rm -rf src target/release/deps/frameshuffle*

COPY src ./src
RUN set -eux; \
    features=""; if [ "$WITH_HEIC" != "1" ]; then features="--no-default-features"; fi; \
    cargo build --release --locked $features

FROM ubuntu:24.04
ARG VERSION=dev
ARG WITH_HEIC=0
LABEL org.opencontainers.image.title="frameshuffle" \
      org.opencontainers.image.description="Local-only shuffled image server for e-paper and other photo frames" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${VERSION}" \
      io.frameshuffle.heic="${WITH_HEIC}"
ENV DEBIAN_FRONTEND=noninteractive
RUN set -eux; \
    pkgs="ca-certificates tzdata curl"; \
    if [ "$WITH_HEIC" = "1" ]; then pkgs="$pkgs libheif1 libheif-plugin-libde265"; fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends $pkgs; \
    rm -rf /var/lib/apt/lists/*; \
    useradd --system --uid 10001 --home /data --shell /usr/sbin/nologin frameshuffle; \
    mkdir -p /data /cache /config; \
    chown frameshuffle /data /cache
COPY --from=build /src/target/release/frameshuffle /usr/local/bin/frameshuffle
USER frameshuffle
ENV FRAMESHUFFLE_CONFIG=/config/config.toml RUST_LOG=info
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=20s CMD curl -fsS http://127.0.0.1:8080/health || exit 1
ENTRYPOINT ["frameshuffle"]
