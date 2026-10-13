# syntax=docker/dockerfile:1
#
# NOTE: this Dockerfile was written carefully but has NOT been built locally
# (Docker is not available on the development machine). See README.md.

# ---- build stage -----------------------------------------------------------
FROM rust:1-bookworm AS build
WORKDIR /src

# Cache dependencies: build a dummy crate with the real manifest first.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src/bin benches \
    && echo "fn main() {}" > src/bin/ledger-server.rs \
    && echo "fn main() {}" > src/bin/ledgerctl.rs \
    && echo "" > src/lib.rs \
    && echo "fn main() {}" > benches/posting.rs \
    && echo "fn main() {}" > benches/replay.rs \
    && cargo build --release --locked --bins \
    && rm -rf src benches

COPY src ./src
COPY benches ./benches
# Touch sources so cargo rebuilds the real crate instead of the dummy.
RUN touch src/lib.rs src/bin/*.rs \
    && cargo build --release --locked --bins

# ---- runtime stage ---------------------------------------------------------
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /var/lib/ledger --create-home ledger

COPY --from=build /src/target/release/ledger-server /usr/local/bin/ledger-server
COPY --from=build /src/target/release/ledgerctl /usr/local/bin/ledgerctl
COPY examples/ledger.toml /etc/ledger/ledger.toml

ENV LEDGER_CONFIG=/etc/ledger/ledger.toml \
    LEDGER_BIND=0.0.0.0:8080 \
    LEDGER_DATA_DIR=/var/lib/ledger/data \
    LEDGER_LOG_FORMAT=json

USER ledger
VOLUME ["/var/lib/ledger/data"]
EXPOSE 8080

HEALTHCHECK --interval=15s --timeout=3s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/ready || exit 1

ENTRYPOINT ["/usr/local/bin/ledger-server"]
