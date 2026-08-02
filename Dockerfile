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
 && apt-get install -y --no-install-recommends ca-certificates curl xz-utils \
 && rm -rf /var/lib/apt/lists/*

# The PDF report shells out to Typst. The devshell wraps it onto the binary's
# PATH for local runs, which does nothing for this image -- so fetch the static
# musl build, pinned to the version the reports were designed against.
ARG TYPST_VERSION=0.15.1
RUN curl -fsSL \
      "https://github.com/typst/typst/releases/download/v${TYPST_VERSION}/typst-x86_64-unknown-linux-musl.tar.xz" \
    | tar -xJ -C /tmp \
 && mv /tmp/typst-x86_64-unknown-linux-musl/typst /usr/local/bin/typst \
 && rm -rf /tmp/typst-x86_64-unknown-linux-musl \
 && apt-get purge -y curl xz-utils && apt-get autoremove -y \
 && typst --version

COPY --from=build /src/target/release/time /usr/local/bin/time

ENV TIME_DATA_DIR=/data \
    TIME_CONFIG=/config/config.toml
EXPOSE 7373
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/time"]
CMD ["server"]
