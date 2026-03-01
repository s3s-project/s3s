FROM rust:latest AS builder

# TARGETARCH is a built-in ARG provided by the Docker builder (e.g., "amd64", "arm64")
# It's automatically available in RUN commands.
ARG TARGETARCH

WORKDIR /app

# Set musl-gcc flags for aws-lc compatibility
ENV CC=musl-gcc \
    CFLAGS="-D__isoc23_sscanf=sscanf -D__isoc23_strtol=strtol"

# Copy source files first to better leverage Docker's layer caching
COPY ./Cargo.toml ./Cargo.toml
COPY ./crates ./crates
COPY ./codegen ./codegen

# This single RUN command handles all platform-specific logic.
# It sets variables, installs dependencies, adds the Rust target, and builds the binary.
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
    && apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        musl \
        musl-dev \
        musl-tools \
        ${GCC_PACKAGE} \
    && rustup target add ${RUST_TARGET} \
    && cargo build --release --target ${RUST_TARGET} --features="binary" --bin s3s-fs --bin s3s-e2e --bin s3s-proxy \
    && rm -rf /var/lib/apt/lists/* \
    && cp target/${RUST_TARGET}/release/s3s-fs target/${RUST_TARGET}/release/s3s-e2e target/${RUST_TARGET}/release/s3s-proxy target/


# Create and set permissions for the data directory
RUN mkdir data && chmod -R 755 data

#----------- FINAL STAGE -----------
FROM scratch

# Copy the statically compiled binary from a known location in the builder stage.
COPY --from=builder /app/target/s3s-fs .
COPY --from=builder /app/target/s3s-e2e .
COPY --from=builder /app/target/s3s-proxy .

# Copy the data directory
COPY --from=builder /app/data /data

# Set the command to run the application
CMD ["./s3s-fs", "--help"]
