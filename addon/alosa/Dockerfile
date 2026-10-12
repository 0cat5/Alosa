# Build from the repository root, not this folder, because the server needs
# the shared protocol crate:
#   docker build -f addon/alosa/Dockerfile -t alosa-server .
FROM docker.io/library/rust:1-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY protocol protocol
COPY crates crates
RUN cargo build --release -p alosa-server

FROM docker.io/library/debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl jq \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/alosa-server /usr/local/bin/alosa-server
COPY addon/alosa/run.sh /run.sh
ENV ALOSA_DATA_DIR=/data
CMD ["/run.sh"]
