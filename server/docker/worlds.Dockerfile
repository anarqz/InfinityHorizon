# syntax=docker/dockerfile:1.27@sha256:bde3983e9c939224420ddaf6b784cc30e09b035a4dea01f581230c50809f372e
#
# The world node (native/worlds): `worlds agent` (the default command) supervises its own
# `worlds serve --profile public` child and registers it with the server. Build context: the
# client repository's root (worlds.Dockerfile.dockerignore keeps it to native/worlds' Cargo.toml,
# Cargo.lock and src/); compose.yml builds it as the `worlds` service.
#
#   docker build -f server/docker/worlds.Dockerfile -t infinity-horizon-worlds:selfhost .
#
# The runtime is distroless (glibc, CA certificates, no shell), runs as nonroot and holds the
# binary, the control socket directory (/run/fi-ds, 0700: the engine refuses a group-readable one)
# and the node key directory (/var/lib/worlds, a volume: the key is generated once there, 0600).
# The crate's rust-toolchain.toml is left out of the context on purpose: the image's compiler is
# the same version and needs none of the Windows targets that file lists.

ARG RUST_IMAGE=rust:1.99-slim-bookworm@sha256:a24e292b4af7bdee351a31d3a6379972a5652f0ea22a1b6d6883324ac4f5097c
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f

FROM ${RUST_IMAGE} AS build
WORKDIR /repo
COPY native/worlds/ ./
RUN --mount=type=cache,id=fi-worlds-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=fi-worlds-target,target=/repo/target,sharing=locked \
    cargo build --release --locked --bin worlds \
 && install -D target/release/worlds /out/bin/worlds \
 && install -d -m 0700 /out/run/fi-ds /out/var/lib/worlds

FROM ${RUNTIME_IMAGE} AS runtime
COPY --from=build /out/bin/worlds /usr/local/bin/worlds
COPY --from=build --chown=nonroot:nonroot /out/run /run
COPY --from=build --chown=nonroot:nonroot /out/var/lib/worlds /var/lib/worlds
USER nonroot:nonroot
ENV MALLOC_ARENA_MAX=2
ENTRYPOINT ["/usr/local/bin/worlds"]
CMD ["agent"]
