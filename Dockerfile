# abproof — offline A/B change-validation harness.
# Build the CLI on the pinned toolchain; runtime carries git + python3 (the
# measured arm shells the execute-node loop). The corpus and loop are provided at
# run time via $ABPROOF_CORPUS and $ABPROOF_EXECUTE_NODE (mount them).
FROM rust:1.94-slim-bookworm AS builder
WORKDIR /build
COPY . .
RUN cargo build --release --bin abproof

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends git python3 ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 abproof
COPY --from=builder /build/target/release/abproof /usr/local/bin/abproof
USER abproof
WORKDIR /work
ENTRYPOINT ["abproof"]
CMD ["run"]
