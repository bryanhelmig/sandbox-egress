FROM rust:1.88.0-slim-bookworm

ENV RUSTUP_TOOLCHAIN=1.88.0

RUN rustup component add clippy rustfmt

WORKDIR /workspace
COPY . .

RUN ./scripts/check-container.sh

CMD ["./scripts/test-conformance.sh"]
