# ─── Stage 1: Rust builder ────────────────────────────────────────────────────
FROM rust:1-bookworm AS builder

WORKDIR /build

# Cache dependency compilation — only reruns when Cargo.toml or Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs
RUN cargo build --release --locked 2>&1 | tail -5
RUN rm -rf src

COPY src ./src
COPY migrations ./migrations
COPY static ./static
COPY config.json ./

# Touch main.rs so cargo re-links against the real source rather than the stub.
RUN touch src/main.rs
RUN cargo build --release --locked

# ─── Stage 2: Runtime ─────────────────────────────────────────────────────────
#
# debian:bookworm-slim, not Alpine/musl — same reason as image-service: proc-macro
# crates need the dynamic linker at build time, and static linking against glibc
# does not work.
FROM debian:bookworm-slim AS runtime

# ffmpeg and ffprobe. Note what they are NOT here for: this service does not encode
# anything — that is MediaConvert's job. They run two bounded operations, probing an
# upload and stream-copying it into a faststart MP4, neither of which touches a
# codec. `--no-install-recommends` keeps the encoder-adjacent extras out; the
# packaged build is still larger than strictly needed, and is chosen for being the
# one Debian keeps patched rather than for being small.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ffmpeg \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /build/target/release/video-service /app/video-service
COPY --from=builder /build/config.json /app/config.json
COPY --from=builder /build/migrations /app/migrations

USER nobody

EXPOSE 3000
HEALTHCHECK --interval=10s --timeout=5s --start-period=15s --retries=3 \
    CMD ["/app/video-service", "healthcheck"]

CMD ["/app/video-service"]
