# Server image only. The agent runs on desktops and needs grim/hyprctl; the
# server needs nothing but the binary and a CA bundle, so this stays small.
FROM rust:1-bookworm AS build
WORKDIR /src

# Cache the dependency build across source-only changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY src ./src
# Cargo skips rebuilding when only mtime changed, so force the real main.rs.
RUN touch src/main.rs && cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/time /usr/local/bin/time

ENV TIME_DATA_DIR=/data \
    TIME_CONFIG=/config/config.toml
EXPOSE 7373
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/time"]
CMD ["server"]
