# syntax=docker/dockerfile:1
#
# The container for relay, written by `leptoskit generate deploy` and edited
# once: the app builds against `crates/quant_relay` in the repository, so the
# build context is the repository root, not this directory. Build it from
# there:
#
#   docker build -f apps/relay/Dockerfile -t relay .
#
# The first stage runs `leptoskit build --release`, as you would on your
# machine. The second runs the server as an unprivileged user that writes
# nothing to the image's filesystem, so it works with a read-only root
# filesystem.
#
# The SQLite database is the one thing it writes, on a volume at /data.

# The Rust release that builds the app. `1` is the latest stable; pin one,
# such as 1.96, to build the same way every time.
ARG RUST_VERSION=1
# The CLI and SDK use the same main commit. Updated by scripts/update-leptoskit.py.
ARG LEPTOSKIT_REV=a8fec995896be1fea532ee589dfb8869c28f1df7

FROM rust:${RUST_VERSION}-bookworm AS build
ARG LEPTOSKIT_REV
RUN cargo install --git https://github.com/leptoskit/leptoskit \
    --rev "${LEPTOSKIT_REV}" --locked leptoskit_cli
WORKDIR /src
# The toolchain file picks the toolchain everything below uses.
COPY apps/relay/rust-toolchain.toml apps/relay/rust-toolchain.toml
# The app's workspace, and the shared crate it builds against by path.
COPY apps/relay apps/relay
COPY crates/quant_relay crates/quant_relay
# The cache mounts keep downloaded crates and compiled dependencies between
# builds. The server is copied out, since the target directory is a cache.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/apps/relay/target \
    --mount=type=cache,target=/src/apps/relay/.leptoskit \
    cd apps/relay \
 && leptoskit build --release --platform server \
 && cp target/release/relay /usr/local/bin/relay

FROM debian:bookworm-slim
# curl answers the health check, and `app` is the unprivileged user the
# server runs as.
RUN apt-get update \
 && apt-get install --yes --no-install-recommends curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --user-group --uid 10001 --no-create-home app \
 && mkdir /data \
 && chown app:app /data
WORKDIR /app
COPY --from=build /usr/local/bin/relay /app/server
COPY --from=build /src/apps/relay/dist /app/dist
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
USER app
EXPOSE 3000
HEALTHCHECK --interval=10s --timeout=3s --start-period=30s --retries=3 \
  CMD ["curl", "--fail", "--silent", "--output", "/dev/null", "http://127.0.0.1:3000/healthz"]
CMD ["/app/server"]
