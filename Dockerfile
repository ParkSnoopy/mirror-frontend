FROM rust:1-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim

COPY --from=builder --chown=65532:65532 /build/target/release/mirror-frontend /usr/local/bin/mirror-frontend

USER 65532:65532
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/mirror-frontend"]
