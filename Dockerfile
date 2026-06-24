# Multi-stage build → small runtime image for the inmem cache server (inmemd).
FROM rust:1-slim AS build
WORKDIR /src
COPY . .
# Portable build (runs on any Linux). The io_uring runtime is an opt-in Linux feature you can
# enable from source with `--features io-uring`.
RUN cargo build --release --bin inmemd

FROM debian:bookworm-slim
RUN useradd -r -u 10001 inmem
COPY --from=build /src/target/release/inmemd /usr/local/bin/inmemd
USER inmem
EXPOSE 6380
# Bind to all interfaces so the container is reachable; override flags as needed.
ENTRYPOINT ["inmemd"]
CMD ["--bind", "0.0.0.0", "--port", "6380"]
