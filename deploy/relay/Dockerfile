# syntax=docker/dockerfile:1
#
# The container for relay, written by `leptoskit generate deploy` and edited
# once: the app builds against `crates/quant_relay` in the repository, so the
# build context is the repository root, not this directory. Build it from
# there:
#
#   docker build -f deploy/relay/Dockerfile -t relay .
#
# The first stage runs `leptoskit build --release`, as you would on your
# machine. The second runs the server as an unprivileged user that writes
# nothing to the image's filesystem, so it works with a read-only root
# filesystem.
#
# The SQLite database is the one thing it writes, on a volume at /data.

# Refresh base digests with `docker buildx imagetools inspect <image>:<tag>`.
ARG RUST_VERSION=1.100.0
ARG RUST_IMAGE_DIGEST=sha256:75d26c23c24cc6ea1cdabf0714a6bff8fd7bf3248716043fd7ae86dac7f41b94
ARG DEBIAN_IMAGE_DIGEST=sha256:e67eeead736ff7bbe258ef088bff683635c561b1fcfefad0806c1df4ab49ce0e
# The CLI and SDK use the same main commit. Updated by scripts/update-leptoskit.py.
ARG LEPTOSKIT_REV=3d2887b72737ef9972a8374a5a6d79b24500ff5a

FROM rust:${RUST_VERSION}-bookworm@${RUST_IMAGE_DIGEST} AS build
ARG LEPTOSKIT_REV
WORKDIR /src
# Install the project's pinned toolchain before building the CLI so both use it.
COPY rust-toolchain.toml ./
RUN rustup show active-toolchain || rustup toolchain install
RUN cargo --version
RUN cargo install --git https://github.com/leptoskit/leptoskit \
    --rev "${LEPTOSKIT_REV}" --locked leptoskit_cli
COPY . .
# The cache mounts keep downloaded crates and compiled dependencies between
# builds. The server is copied out, since the target directory is a cache.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/src/.leptoskit \
    leptoskit --app-target relay build --release \
 && cp target/release/relay /usr/local/bin/relay

FROM debian:bookworm-slim@${DEBIAN_IMAGE_DIGEST}
# curl answers the health check, and `app` is the unprivileged user the
# server runs as, with uid and gid 10001.
RUN apt-get update \
 && apt-get install --yes --no-install-recommends curl \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid 10001 app \
 && useradd --system --uid 10001 --gid app --no-create-home app \
 && mkdir /data \
 && chown app:app /data
WORKDIR /app
COPY --from=build /usr/local/bin/relay /app/server
COPY --from=build /src/dist/relay /app/dist
# What `leptoskit serve` sets from leptoskit.toml, for a container: listen on
# every interface, and serve the site from /app/dist.
ENV LEPTOS_ENV=PROD \
    LEPTOS_OUTPUT_NAME=relay \
    LEPTOS_SITE_ROOT=/app/dist \
    LEPTOS_SITE_PKG_DIR=pkg \
    LEPTOS_SITE_ADDR=0.0.0.0:3000
# The SQLite database lives on a volume, the one place the server writes.
ENV DATABASE_URL=sqlite:///data/relay.sqlite3
VOLUME /data
# Numeric, so hosts that require a non-root user by id (Kubernetes runAsNonRoot) accept it.
USER 10001:10001
EXPOSE 3000
HEALTHCHECK --interval=10s --timeout=3s --start-period=30s --retries=3 \
  CMD ["curl", "--fail", "--silent", "--output", "/dev/null", "http://127.0.0.1:3000/healthz"]
CMD ["/app/server"]
