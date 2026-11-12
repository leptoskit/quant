# syntax=docker/dockerfile:1
#
# The container for the landing page, written by `leptoskit generate deploy`
# and edited once: the CLI is installed from the same leptoskit commit the app
# pins, not from crates.io. Build it from this directory:
#
#   docker build -f deploy/landing/Dockerfile -t landing .
#
# The first stage runs `leptoskit build --release`, as you would on your
# machine. The second runs the server as an unprivileged user that writes
# nothing to the image's filesystem, so it works with a read-only root
# filesystem.

# The Rust release that builds the app. `1` is the latest stable; pin one,
# such as 1.96, to build the same way every time.
ARG RUST_VERSION=1
# The CLI and SDK use the same main commit. Updated by scripts/update-leptoskit.py.
ARG LEPTOSKIT_REV=67165da4842b729de1ea43dee531c94d613df217
# binaryen's wasm-opt shrinks the release client.
ARG BINARYEN_VERSION=132

FROM rust:${RUST_VERSION}-bookworm AS build
ARG LEPTOSKIT_REV
ARG BINARYEN_VERSION
RUN curl --fail --silent --show-error --location --retry 5 --retry-all-errors \
      --output /tmp/binaryen.tar.gz \
      "https://github.com/WebAssembly/binaryen/releases/download/version_${BINARYEN_VERSION}/binaryen-version_${BINARYEN_VERSION}-$(uname -m)-linux.tar.gz" \
 && tar -xzf /tmp/binaryen.tar.gz -C /opt \
 && rm /tmp/binaryen.tar.gz \
 && ln -s "/opt/binaryen-version_${BINARYEN_VERSION}/bin/wasm-opt" /usr/local/bin/wasm-opt
RUN cargo install --git https://github.com/leptoskit/leptoskit \
    --rev "${LEPTOSKIT_REV}" --locked leptoskit_cli
WORKDIR /src
# The toolchain file picks the toolchain everything below uses.
COPY . .
RUN rustup show active-toolchain
# The cache mounts keep downloaded crates and compiled dependencies between
# builds. The server is copied out, since the target directory is a cache.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/src/.leptoskit \
    leptoskit --app-target landing build --release \
 && cp target/release/landing /usr/local/bin/landing

FROM debian:bookworm-slim
# curl answers the health check, and `app` is the unprivileged user the
# server runs as, with uid and gid 10001.
RUN apt-get update \
 && apt-get install --yes --no-install-recommends curl \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid 10001 app \
 && useradd --system --uid 10001 --gid app --no-create-home app
WORKDIR /app
COPY --from=build /usr/local/bin/landing /app/server
COPY --from=build /src/dist/landing /app/dist
# What `leptoskit serve` sets from leptoskit.toml, for a container: listen on
# every interface, and serve the site from /app/dist.
ENV LEPTOS_ENV=PROD \
    LEPTOS_OUTPUT_NAME=landing_app \
    LEPTOS_SITE_ROOT=/app/dist \
    LEPTOS_SITE_PKG_DIR=pkg \
    LEPTOS_SITE_ADDR=0.0.0.0:3000
USER app
EXPOSE 3000
HEALTHCHECK --interval=10s --timeout=3s --start-period=30s --retries=3 \
  CMD ["curl", "--fail", "--silent", "--output", "/dev/null", "http://127.0.0.1:3000/healthz"]
CMD ["/app/server"]
