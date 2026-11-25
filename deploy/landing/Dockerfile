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

# Refresh base digests with `docker buildx imagetools inspect <image>:<tag>`.
ARG RUST_VERSION=1.100.0
ARG RUST_IMAGE_DIGEST=sha256:75d26c23c24cc6ea1cdabf0714a6bff8fd7bf3248716043fd7ae86dac7f41b94
ARG DEBIAN_IMAGE_DIGEST=sha256:e67eeead736ff7bbe258ef088bff683635c561b1fcfefad0806c1df4ab49ce0e
# The CLI and SDK use the same main commit. Updated by scripts/update-leptoskit.py.
ARG LEPTOSKIT_REV=2a8294b2f569ff7f1f1bd7d7035e458aa06c0a8c
# binaryen's wasm-opt shrinks the release client, and esbuild minifies its
# JavaScript glue.
ARG BINARYEN_VERSION=132
ARG BINARYEN_SHA256_X86_64=195ddc94f9bc89f45abdabb0b9eea86023d727ba90eac8b35b80f2544fc30572
ARG BINARYEN_SHA256_AARCH64=c58562417836c5d0493d89bdefc434933bdc097db641b483df86bcfa557a107f
ARG ESBUILD_VERSION=0.28.2
ARG ESBUILD_SHA256_X86_64=9573bb2233aab0f9ea7647d5cca9726113cc1768de61d66b17267f4db84488f6
ARG ESBUILD_SHA256_AARCH64=a96dbfa41d3ef5dbd1ef22b1c10d5187be9267e86093a870f06402a7ec931596

FROM rust:${RUST_VERSION}-bookworm@${RUST_IMAGE_DIGEST} AS build
ARG LEPTOSKIT_REV
WORKDIR /src
# Install the project's pinned toolchain before building the CLI so both use it.
COPY rust-toolchain.toml ./
RUN rustup show active-toolchain || rustup toolchain install
RUN cargo --version
RUN cargo install --git https://github.com/leptoskit/leptoskit \
    --rev "${LEPTOSKIT_REV}" --locked leptoskit_cli
ARG BINARYEN_VERSION
ARG BINARYEN_SHA256_X86_64
ARG BINARYEN_SHA256_AARCH64
ARG ESBUILD_VERSION
ARG ESBUILD_SHA256_X86_64
ARG ESBUILD_SHA256_AARCH64
RUN case "$(uname -m)" in \
      x86_64) esbuild=linux-x64; sha256="${ESBUILD_SHA256_X86_64}" ;; \
      aarch64) esbuild=linux-arm64; sha256="${ESBUILD_SHA256_AARCH64}" ;; \
      *) exit 1 ;; \
    esac \
 && curl --fail --silent --show-error --location --retry 5 --retry-all-errors \
      --output /tmp/esbuild.tgz \
      "https://registry.npmjs.org/@esbuild/${esbuild}/-/${esbuild}-${ESBUILD_VERSION}.tgz" \
 && printf '%s  /tmp/esbuild.tgz\n' "${sha256}" | sha256sum --check --strict \
 && tar -xzf /tmp/esbuild.tgz -C /tmp \
 && install -m 0755 /tmp/package/bin/esbuild /usr/local/bin/esbuild \
 && rm -rf /tmp/esbuild.tgz /tmp/package
RUN case "$(uname -m)" in \
      x86_64) arch=x86_64; sha256="${BINARYEN_SHA256_X86_64}" ;; \
      aarch64) arch=aarch64; sha256="${BINARYEN_SHA256_AARCH64}" ;; \
      *) exit 1 ;; \
    esac \
 && curl --fail --silent --show-error --location --retry 5 --retry-all-errors \
      --output /tmp/binaryen.tar.gz \
      "https://github.com/WebAssembly/binaryen/releases/download/version_${BINARYEN_VERSION}/binaryen-version_${BINARYEN_VERSION}-${arch}-linux.tar.gz" \
 && printf '%s  /tmp/binaryen.tar.gz\n' "${sha256}" | sha256sum --check --strict \
 && tar -xzf /tmp/binaryen.tar.gz -C /opt \
 && rm /tmp/binaryen.tar.gz \
 && ln -s "/opt/binaryen-version_${BINARYEN_VERSION}/bin/wasm-opt" /usr/local/bin/wasm-opt
COPY . .
# The cache mounts keep downloaded crates and compiled dependencies between
# builds. The server is copied out, since the target directory is a cache.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/src/.leptoskit \
    leptoskit --app-target landing build --release \
 && cp target/release/landing /usr/local/bin/landing

FROM debian:bookworm-slim@${DEBIAN_IMAGE_DIGEST}
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
# Numeric, so hosts that require a non-root user by id (Kubernetes runAsNonRoot) accept it.
USER 10001:10001
EXPOSE 3000
HEALTHCHECK --interval=10s --timeout=3s --start-period=30s --retries=3 \
  CMD ["curl", "--fail", "--silent", "--output", "/dev/null", "http://127.0.0.1:3000/healthz"]
CMD ["/app/server"]
