# Docker

The s3s project provides Docker images for the following binaries:
- `s3s-fs`: File system-based S3 implementation
- `s3s-e2e`: End-to-end testing binary
- `s3s-proxy`: Proxy implementation

## Image Location

Docker images are published to **GitHub Container Registry (GHCR)** at:

```
ghcr.io/s3s-project/s3s
```

## Available Tags

- **Version tags**: `v0.12.0`, `0.12.0`, `0.12`, `0` (for released versions)
- **`latest`**: Latest stable release
- **`edge`**: Weekly builds from the main branch (updated every Sunday)

## Platforms

Images are built for the following platforms:
- `linux/amd64` (x86_64)
- `linux/arm64` (ARM64/aarch64)

## Usage

### Pulling an Image

```bash
# Pull the latest stable version
docker pull ghcr.io/s3s-project/s3s:latest

# Pull a specific version
docker pull ghcr.io/s3s-project/s3s:v0.12.0

# Pull the edge build
docker pull ghcr.io/s3s-project/s3s:edge
```

### Running a Container

The default command shows help for `s3s-fs`:

```bash
docker run --rm ghcr.io/s3s-project/s3s:latest
```

To run `s3s-fs` server:

```bash
docker run --rm -p 8014:8014 ghcr.io/s3s-project/s3s:latest ./s3s-fs --host 0.0.0.0 --port 8014
```

To run other binaries:

```bash
# s3s-proxy
docker run --rm ghcr.io/s3s-project/s3s:latest ./s3s-proxy --help

# s3s-e2e
docker run --rm ghcr.io/s3s-project/s3s:latest ./s3s-e2e --help
```

## Image Details

- **Base image**: `scratch` (minimal, static binaries)
- **Binaries**: Statically compiled with musl for minimal size
- **Size**: Optimized for small footprint
- **Security**: No shell or unnecessary tools included

## Building Locally

To build the Docker image locally:

```bash
docker build -f docker/Dockerfile -t s3s:local .
```

## Migration from Docker Hub

**Note**: Docker images were previously published to Docker Hub. As of the migration, all new releases are published to GitHub Container Registry (ghcr.io). Please update your scripts and configurations to use the new registry location.

Old location (deprecated):
```
<username>/s3s
```

New location:
```
ghcr.io/s3s-project/s3s
```
