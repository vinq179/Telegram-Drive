FROM rust:1.88-bookworm AS builder
WORKDIR /src
COPY headless/Cargo.toml ./Cargo.toml
COPY headless/src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo test --release \
    && cargo build --release \
    && cp /src/target/release/telegram-drive-headless /telegram-drive-headless

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home telegram-drive \
    && mkdir -p /data \
    && chown telegram-drive:telegram-drive /data
COPY --from=builder /telegram-drive-headless /usr/local/bin/telegram-drive-headless
USER telegram-drive
ENV TELEGRAM_SESSION_PATH=/data/telegram.session \
    TELEGRAM_DRIVE_BIND=0.0.0.0:8550 \
    RUST_LOG=info
VOLUME ["/data"]
EXPOSE 8550
ENTRYPOINT ["/usr/local/bin/telegram-drive-headless"]
CMD ["serve"]
