# syntax=docker/dockerfile:1.27@sha256:bde3983e9c939224420ddaf6b784cc30e09b035a4dea01f581230c50809f372e
#
# The UDP relay (fi-relay-vm): 3478 (binding answers) and 5101 (rendezvous and the capped
# fallback). Build context: the server folder (relay.Dockerfile.dockerignore keeps it to the Cargo
# workspace); compose.yml builds it for the `relay` profile.
#
#   docker build -f docker/relay.Dockerfile -t infinity-horizon-relay:selfhost .
#
# A locked release build of one package, then distroless (no shell, no package manager) as
# nonroot. It needs no database; its configuration is FI_RELAY_* and the relay-grant verify ring
# (FI_RELAY_GRANT_VERIFY_KEYS, or the keys volume through FI_KEYS_DIR).

ARG RUST_IMAGE=rust:1.99-slim-bookworm@sha256:a24e292b4af7bdee351a31d3a6379972a5652f0ea22a1b6d6883324ac4f5097c
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f

FROM ${RUST_IMAGE} AS build
WORKDIR /repo
COPY . .
RUN --mount=type=cache,id=fi-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=fi-cargo-target,target=/repo/target,sharing=locked \
    cargo build --release --locked --package fi-relay-vm --bin fi-relay-vm \
 && install -D target/release/fi-relay-vm /out/fi-relay-vm

FROM ${RUNTIME_IMAGE} AS runtime
COPY --from=build /out/fi-relay-vm /usr/local/bin/fi-relay-vm
USER nonroot:nonroot
ENV RUST_LOG=info
EXPOSE 3478/udp 5101/udp
ENTRYPOINT ["/usr/local/bin/fi-relay-vm"]
