FROM rust:1.83-slim AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=builder /app/target/release/shm-server /usr/local/bin/
COPY --from=builder /app/target/release/shm-client /usr/local/bin/

# Server listens on /dev/shm by default on Linux
ENTRYPOINT ["shm-server"]
