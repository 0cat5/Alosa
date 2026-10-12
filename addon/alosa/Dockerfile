# Alosa server add-on image. Build from the repository root, because the
# server needs the shared protocol crate:
#
#   docker buildx build --platform linux/arm64 -f addon/alosa/Dockerfile -t alosa-addon .
#
# The build stage runs on the build machine's own architecture and
# cross-compiles a static binary for the target, which is much faster than
# compiling under emulation. Everything is pure Rust, so Rust's bundled linker
# is all it needs.
FROM --platform=$BUILDPLATFORM docker.io/library/rust:1-slim-trixie AS build
ARG TARGETARCH
WORKDIR /src
RUN case "$TARGETARCH" in \
        amd64) echo x86_64-unknown-linux-musl ;; \
        arm64) echo aarch64-unknown-linux-musl ;; \
        *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac > /rust-target \
    && rustup target add "$(cat /rust-target)"
COPY Cargo.toml Cargo.lock ./
COPY protocol protocol
COPY crates crates
RUN RUSTFLAGS="-C linker=rust-lld" cargo build --release --locked -p alosa-server \
        --target "$(cat /rust-target)" \
    && cp "target/$(cat /rust-target)/release/alosa-server" /alosa-server

FROM docker.io/library/alpine:3
RUN apk add --no-cache curl jq
COPY --from=build /alosa-server /usr/local/bin/alosa-server
COPY addon/alosa/run.sh /run.sh
ENV ALOSA_DATA_DIR=/data
CMD ["/run.sh"]
