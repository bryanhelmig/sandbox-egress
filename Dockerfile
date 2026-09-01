FROM rust:1.88.0-slim-bookworm AS factory

ENV RUSTUP_TOOLCHAIN=1.88.0

RUN rustup component add clippy rustfmt

WORKDIR /workspace

COPY Cargo.toml Cargo.lock ./
RUN printf '# Sandbox Egress dependency cache\n' > README.md \
    && mkdir -p src benches tests \
    && printf '//! Container dependency cache placeholder.\n' > src/lib.rs \
    && printf '//! Container dependency cache placeholder.\n' > tests/resource_soak.rs \
    && printf '//! Container dependency cache placeholder.\nfn main() {}\n' > benches/connections.rs \
    && printf '//! Container dependency cache placeholder.\nfn main() {}\n' > benches/lifecycle.rs \
    && cargo check --locked --all-targets --all-features \
    && cargo clippy --locked --all-targets --all-features -- -D warnings \
    && cargo test --locked --all-targets --all-features --no-run \
    && RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features \
    && cargo test --locked --release --test resource_soak --no-run \
    && rm -rf src benches tests README.md

COPY . .

RUN find src tests benches -type f -exec touch {} + \
    && ./scripts/check-container.sh \
    && ./scripts/collect-conformance-binaries.sh /conformance

FROM debian:bookworm-slim AS conformance

COPY --from=factory /conformance/bin /conformance/bin
COPY --from=factory /conformance/run.sh /conformance/run.sh
COPY --from=factory /conformance/sandbox-egress /workspace/target/debug/sandbox-egress

WORKDIR /conformance
USER 65534:65534

CMD ["./run.sh"]
