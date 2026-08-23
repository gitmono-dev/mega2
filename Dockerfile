# Build monoengine for the IT compose stack.
#
# All path deps (monoengine, bin, crates/orbit, crates/orbit-api) live inside
# the `monoengine/` directory in the build context, so only that directory is
# copied. From the monoengine repo root:
#
#   docker compose -p monoengine-it -f docker-compose.test.yml --profile app build monoengine
#
# (compose sets `build.context: ..` and `dockerfile: monoengine/Dockerfile`.)

FROM rust:1.97-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        clang \
        cmake \
        git \
        libssl-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY monoengine /src/monoengine

WORKDIR /src/monoengine
ARG TARGETARCH
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=monoengine-it-cargo-registry-${TARGETARCH},sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,id=monoengine-it-cargo-git-${TARGETARCH},sharing=locked \
    --mount=type=cache,target=/src/monoengine/target,id=monoengine-it-target-${TARGETARCH},sharing=locked \
    cargo build --release -p monoengine \
    && cp /src/monoengine/target/release/monoengine /usr/local/bin/monoengine

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/monoengine /usr/local/bin/monoengine
COPY monoengine/config/config.toml /etc/monoengine/config.toml

ENV MEGA_BASE_DIR=/var/lib/monoengine \
    MEGA_CONFIG=/etc/monoengine/config.toml

EXPOSE 8000
ENTRYPOINT ["/usr/local/bin/monoengine"]
CMD ["--config", "/etc/monoengine/config.toml", "service", "http", "--host", "0.0.0.0", "-p", "8000"]
