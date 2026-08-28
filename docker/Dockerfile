# TARGETARCH is a built-in ARG provided by the Docker builder (e.g., "amd64", "arm64").
# It is automatically available in RUN commands.

# ----------- BASE / TOOLCHAIN STAGE -----------
# This layer depends only on the base image and TARGETARCH, never on the source
# tree, so editing Rust code does not invalidate the (expensive) toolchain setup.
# It installs the musl cross-compilation toolchain, the Rust musl target and
# cargo-chef, which plans and pre-builds the dependency graph in later stages.
FROM rust:latest AS chef

ARG TARGETARCH

WORKDIR /app

# Set musl-gcc flags for aws-lc compatibility.
ENV CC=musl-gcc \
    CFLAGS="-D__isoc23_sscanf=sscanf -D__isoc23_strtol=strtol"

# Resolve the Rust target / cross-gcc for the requested architecture, install the
# musl toolchain, add the Rust target and install cargo-chef. The resolved target
# triple is persisted to /rust_target so later stages can reuse it.
RUN \
    case ${TARGETARCH} in \
        "amd64") \
            RUST_TARGET="x86_64-unknown-linux-musl" \
            && GCC_PACKAGE="gcc-x86-64-linux-gnu" \
            ;; \
        "arm64") \
            RUST_TARGET="aarch64-unknown-linux-musl" \
            && GCC_PACKAGE="gcc-aarch64-linux-gnu" \
            ;; \
        *) \
            echo "Unsupported architecture: ${TARGETARCH}" >&2 \
            && exit 1 \
            ;; \
    esac \
    && echo "${RUST_TARGET}" > /rust_target \
    && apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        musl \
        musl-dev \
        musl-tools \
        ${GCC_PACKAGE} \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add "${RUST_TARGET}" \
    && cargo install cargo-chef --locked

# ----------- PLANNER STAGE -----------
# Compute the dependency recipe. Only the workspace manifests, the lockfile and
# the sources required for workspace-member validation are copied here; the
# recipe itself only changes when the dependency graph changes.
FROM chef AS planner

COPY ./Cargo.toml ./Cargo.toml
COPY ./Cargo.lock ./Cargo.lock
COPY ./crates ./crates
COPY ./codegen ./codegen

RUN cargo chef prepare --recipe-path recipe.json

# ----------- BUILDER STAGE -----------
FROM chef AS builder

# Cook (build) only the dependencies described by the recipe. This layer is keyed
# by recipe.json alone, so source-only changes reuse the compiled dependencies
# via the layer cache. --locked keeps the resolved versions reproducible.
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --locked --target "$(cat /rust_target)" \
        --features="binary" --bin s3s-fs --bin s3s-e2e --bin s3s-proxy \
        --recipe-path recipe.json

# Copy the real sources (including the committed Cargo.lock for reproducible,
# drift-detecting builds) and build the workspace binaries. codegen is present
# only for workspace-member validation; generated files are committed, so no
# codegen runs at build time.
COPY ./Cargo.toml ./Cargo.toml
COPY ./Cargo.lock ./Cargo.lock
COPY ./crates ./crates
COPY ./codegen ./codegen

RUN RUST_TARGET="$(cat /rust_target)" \
    && cargo build --release --locked --target "${RUST_TARGET}" \
        --features="binary" --bin s3s-fs --bin s3s-e2e --bin s3s-proxy \
    && cp "target/${RUST_TARGET}/release/s3s-fs" \
          "target/${RUST_TARGET}/release/s3s-e2e" \
          "target/${RUST_TARGET}/release/s3s-proxy" target/

# Create and set permissions for the data directory.
RUN mkdir data && chmod -R 755 data

# ----------- FINAL STAGE -----------
FROM scratch

# Copy the statically compiled binaries from a known location in the builder stage.
COPY --from=builder /app/target/s3s-fs .
COPY --from=builder /app/target/s3s-e2e .
COPY --from=builder /app/target/s3s-proxy .

# Copy the data directory.
COPY --from=builder /app/data /data

# Set the command to run the application.
CMD ["./s3s-fs", "--help"]
