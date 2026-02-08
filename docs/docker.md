# Docker Images

The s3s project provides official Docker images with pre-built binaries for easy deployment and testing.

## Available Images

Docker images are published to Docker Hub. The registry location is configured in the GitHub Actions workflow and will be announced when the first release is published.

> **Note**: Check the [GitHub releases page](https://github.com/s3s-project/s3s/releases) for Docker image availability and the exact image name.

### Image Tags

The following tags are available:

- **Version tags** (e.g., `0.12.0`, `0.12`, `0`): Published when a new version is released (tags matching `v*`)
  - `<major>.<minor>.<patch>`: Full semantic version (e.g., `0.12.0`)
  - `<major>.<minor>`: Major and minor version (e.g., `0.12`)
  - `<major>`: Major version only (e.g., `0`)
- **`latest`**: Points to the most recent stable release
- **`edge`**: Built weekly (Sundays at 00:00 UTC) from the `main` branch, contains the latest development changes

### Supported Platforms

Multi-platform images are built for:
- `linux/amd64` (x86_64)
- `linux/arm64` (ARM64/aarch64)

## Included Binaries

Each Docker image contains the following statically-linked binaries:

- **`s3s-fs`**: S3-compatible file system implementation
  - Sample implementation for integration testing
  - Can be used to mock an S3 client
  - Provides debugging capabilities
- **`s3s-e2e`**: End-to-end testing binary
- **`s3s-proxy`**: S3 proxy implementation for testing

## Usage Examples

### Running s3s-fs

The default command shows help information:

```bash
docker run --rm <image>:latest
```

To run the s3s-fs server with custom configuration:

```bash
docker run -d \
  -p 8014:8014 \
  -v $(pwd)/data:/data \
  --name s3s-fs \
  <image>:latest \
  ./s3s-fs \
  --host 0.0.0.0 \
  --port 8014 \
  --access-key AKEXAMPLES3S \
  --secret-key SKEXAMPLES3S \
  --domain-name localhost:8014 \
  --fs-root /data
```

Replace `<image>` with the actual Docker image name from the releases page.

### Using Different Binaries

To run a different binary from the image:

```bash
# Run s3s-proxy
docker run --rm <image>:latest ./s3s-proxy --help

# Run s3s-e2e
docker run --rm <image>:latest ./s3s-e2e --help
```

### Using the Edge Tag

To use the latest development version:

```bash
docker pull <image>:edge
docker run --rm <image>:edge ./s3s-fs --help
```

### Docker Compose Example

Example `docker-compose.yml`:

```yaml
version: '3.8'

services:
  s3s-fs:
    image: <image>:latest
    command: >
      ./s3s-fs
      --host 0.0.0.0
      --port 8014
      --access-key AKEXAMPLES3S
      --secret-key SKEXAMPLES3S
      --domain-name localhost:8014
      --fs-root /data
    ports:
      - "8014:8014"
    volumes:
      - ./data:/data
```

Replace `<image>` with the actual Docker image name from the releases page.

## Build Information

Images are built from the [`docker/Dockerfile`](../docker/Dockerfile) using:
- **Base image**: `rust:1.89` for building
- **Final image**: `scratch` (minimal, statically-linked binaries)
- **Build target**: `musl` for static linking
- **Features**: Binaries are built with the `binary` feature flag

The build process is automated via GitHub Actions in [`.github/workflows/docker.yml`](../.github/workflows/docker.yml).

## Release Schedule

- **Version releases**: Published whenever a new version tag (matching `v*`) is pushed to the repository
- **Edge releases**: Built automatically every Sunday at 00:00 UTC from the `main` branch
- **Manual builds**: Can be triggered via GitHub Actions workflow dispatch

## Security Considerations

⚠️ **Important**: The S3 service adapters in these images have no built-in security protection. When deploying to production or exposing to the Internet, you must implement:

- HTTP body length limits
- Rate limiting
- Back pressure handling
- Authentication and authorization
- Network security (firewalls, VPNs, etc.)

See the main [README](../README.md#security) for more security information.

## Getting Help

For issues, questions, or contributions related to Docker images:
- [GitHub Issues](https://github.com/s3s-project/s3s/issues)
- [Development Guide](../CONTRIBUTING.md)

## Alternative Installation Methods

If Docker is not suitable for your use case, you can also:
- Install from source: See [CONTRIBUTING.md](../CONTRIBUTING.md)
- Install from crates.io: `cargo install s3s-fs --features binary`
