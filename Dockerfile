# syntax=docker/dockerfile:1

# --- Build ---
FROM rust:1-bookworm AS build
WORKDIR /src
# libdbus for the chest strap backend (BlueZ over D-Bus).
RUN apt-get update && apt-get install -y --no-install-recommends libdbus-1-dev pkg-config     && rm -rf /var/lib/apt/lists/*

# Build dependencies first so they stay cached while only the sources change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
COPY web ./web
# Declared after the dependency layer so a new commit does not invalidate it.
ARG KETTLER_GIT_SHA=dev
ENV KETTLER_GIT_SHA=$KETTLER_GIT_SHA
RUN touch src/main.rs && cargo build --release

# --- Runtime ---
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends libdbus-1-3     && rm -rf /var/lib/apt/lists/*
# Non-root user with fixed uid 10001 (chown bind mounts to it); dialout grants access to /dev/ttyUSB0.
RUN useradd --system --uid 10001 --create-home --groups dialout kettler \
    && mkdir /data && chown 10001:10001 /data

COPY --from=build /src/target/release/kettler /usr/local/bin/kettler

USER kettler
ENV KETTLER_DB=/data/kettler.db
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["kettler","server","--listen","0.0.0.0:8080"]
