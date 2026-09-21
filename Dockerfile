# Build mega2 for the compose stacks (IT + storage-only).
#
# Build context = this repository root (not the parent). From the mega2
# repo root:
#
#   docker compose -p mega2-it -f docker/docker-compose.test.yml --profile app build mega2
#   docker compose -p mega2-trunk -f docker-compose.storage-only.yml build mega2
#
# `.dockerignore` excludes `target/` and other host artifacts so they are never
# sent in the build context.

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

WORKDIR /src/mega2
COPY . /src/mega2

ARG TARGETARCH
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=mega2-it-cargo-registry-${TARGETARCH},sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,id=mega2-it-cargo-git-${TARGETARCH},sharing=locked \
    --mount=type=cache,target=/src/mega2/target,id=mega2-it-target-${TARGETARCH},sharing=locked \
    cargo build --release -p mega2 \
    && cp /src/mega2/target/release/mega2 /usr/local/bin/mega2

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/mega2 /usr/local/bin/mega2
COPY config/config.toml /etc/mega2/config.toml

ENV MEGA_BASE_DIR=/var/lib/mega2 \
    MEGA_CONFIG=/etc/mega2/config.toml

EXPOSE 8000
ENTRYPOINT ["/usr/local/bin/mega2"]
CMD ["--config", "/etc/mega2/config.toml", "service", "http", "--host", "0.0.0.0", "-p", "8000"]
