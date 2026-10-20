FROM rust:1.91.0 AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Build for release
RUN cargo build --release --locked

# Runtime image
FROM debian:bookworm-slim

# Install OpenSSL (needed for reqwest), and tzdata so TZ names a real timezone
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    tzdata \
    && rm -rf /var/lib/apt/lists/*

# Schedule times are local wall-clock time in this timezone; `-e TZ=...` sets yours
ENV TZ=Europe/London

WORKDIR /app

# Copy binary from builder
COPY --from=builder /app/target/release/ha-heating-scheduler .

EXPOSE 3000

CMD ["./ha-heating-scheduler"]