# syntax=docker/dockerfile:1
FROM rust:1.98.0-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY protocol/ ./protocol/
COPY server/ ./server/
COPY client/Cargo.toml ./client/
COPY client/src/ ./client/src/
RUN cargo build --release --locked --package maggotdrop-server

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 maggotdrop \
    && useradd --system --uid 10001 --gid maggotdrop --no-create-home maggotdrop \
    && install -d -m 0750 -o maggotdrop -g maggotdrop /data
COPY --from=build /build/target/release/maggotdrop-server /usr/local/bin/maggotdrop-server
ENV MAGGOTDROP_BIND=0.0.0.0:3000 \
    MAGGOTDROP_DATA_DIR=/data
VOLUME ["/data"]
EXPOSE 3000
USER 10001:10001
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD ["curl", "--fail", "--silent", "--output", "/dev/null", "http://127.0.0.1:3000/"]
ENTRYPOINT ["/usr/local/bin/maggotdrop-server"]
