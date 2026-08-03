FROM rust:slim-trixie AS builder

# Lore's build scripts generate gRPC code, which needs protoc.
RUN apt-get update && apt-get install -y \
    build-essential \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release && \
    cp /build/target/release/lore-tui /build/lore-tui-bin

FROM debian:trixie-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/lore-tui-bin /usr/local/bin/lore-tui

WORKDIR /work
ENTRYPOINT ["lore-tui"]
