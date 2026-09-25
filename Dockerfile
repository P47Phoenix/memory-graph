# syntax=docker/dockerfile:1.7
#
# memory-graph as a container: a static musl binary on an empty base.
#
# The binary is cross-compiled on the build host for the requested
# platform (linux/amd64 or linux/arm64), so a multi-arch image needs no
# QEMU: the workspace is pure Rust (scripts/check-no-c-deps.py), so
# rust-lld can link the self-contained musl target for either architecture.
#
#   docker build -t memory-graph .
#   docker run --rm -v "$PWD:/src:ro" -v mg-data:/data memory-graph index --org acme --repo api /src
#
# The default database path is ./graph.redb, and the working directory is
# /data, so the database lands on the /data volume unless --db says otherwise.

FROM --platform=$BUILDPLATFORM rust:1-bookworm AS build
ARG TARGETARCH
WORKDIR /src

# Map the OCI architecture to the static musl target and install it.
RUN case "$TARGETARCH" in \
      amd64) t=x86_64-unknown-linux-musl ;; \
      arm64) t=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH=$TARGETARCH (linux/amd64 and linux/arm64 are built)" >&2; exit 1 ;; \
    esac \
 && echo "$t" > /rust-target \
 && rustup target add "$t" \
 && mkdir -p /data

# Only what cargo needs to resolve and build the workspace (.dockerignore
# keeps the rest out of the context).
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY examples ./examples

# rust-lld ships with the toolchain (next to the host's rustlib) but is not
# on PATH; link-self-contained brings rustc's own musl crt objects so no
# cross gcc is needed. Registry and target caches are BuildKit cache mounts.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,id=memory-graph-target-$TARGETARCH \
    t="$(cat /rust-target)" \
 && export PATH="$(rustc --print target-libdir)/../bin:$PATH" \
 && export RUSTFLAGS="-C linker=rust-lld -C link-self-contained=yes -C target-feature=+crt-static" \
 && cargo build --release --locked -p graph-cli --target "$t" \
 && cp "target/$t/release/memory-graph" /memory-graph

FROM scratch AS runtime
LABEL org.opencontainers.image.source="https://github.com/P47Phoenix/memory-graph" \
      org.opencontainers.image.description="Embedded graph database for source code: org, repo, file, symbol and token nodes with exact spans, in one redb file" \
      org.opencontainers.image.licenses="Apache-2.0"
COPY --from=build /memory-graph /memory-graph
# /data is owned by the runtime user so a fresh named volume (which copies
# the image directory's ownership) is writable. A bind mount keeps the
# host's ownership: pass --user to match it.
COPY --from=build --chown=65532:65532 /data /data
USER 65532:65532
WORKDIR /data
VOLUME ["/data"]
ENTRYPOINT ["/memory-graph"]
CMD ["--help"]
