FROM rust:1-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
COPY migrations ./migrations

RUN cargo build --release

FROM debian:bookworm-slim

RUN adduser --disabled-password --gecos "" appuser

COPY --from=builder /app/target/release/simplefin-server /usr/local/bin/simplefin-server

USER appuser

EXPOSE 8080

CMD ["simplefin-server"]
