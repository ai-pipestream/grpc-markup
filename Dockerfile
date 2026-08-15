# Build stage: compile and run the whole suite. An image never ships from a
# tree whose tests did not pass, so `cargo test` is a build step and not a
# separate CI job that an image could be built around.
FROM rust:1.97-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
      protobuf-compiler ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo test --release \
    && cargo build --release --bin grpc-markup \
    && strip target/release/grpc-markup

# Runtime: the binary and nothing else. No shell is needed to run it, but
# debian-slim keeps one available for an operator, matching the fleet's other
# Rust images; the process itself never execs anything.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --shell /usr/sbin/nologin grpcmarkup
COPY --from=build /src/target/release/grpc-markup /usr/local/bin/grpc-markup

# The server is diskless by doctrine: document bytes live in memory between
# the request stream and the folds, nothing is written, and no library
# spills. `docker run --read-only` therefore works with no tmpfs mount at all.
USER grpcmarkup
EXPOSE 50051
ENV GRPC_MARKUP_ADDR=0.0.0.0:50051

# grpc.health.v1.Health is registered, so an orchestrator probes the service
# rather than the port. There is no HTTP endpoint and no curl in the image;
# use grpc_health_probe or the runtime's native gRPC probe.
ENTRYPOINT ["/usr/local/bin/grpc-markup"]
