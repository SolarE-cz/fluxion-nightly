# Building Docker Images with Nix

This document describes how to build FluxION Home Assistant addon Docker images using Nix instead of
traditional Docker builds.

## Benefits

- **Reproducible builds**: Nix ensures bit-for-bit reproducible builds
- **Cross-compilation**: Build ARM images on x86_64 without emulation
- **Caching**: Nix caches build artifacts efficiently
- **No Docker-in-Docker**: Build images without running Docker daemon
- **Offline builds**: All dependencies are fetched upfront

## Prerequisites

- Nix with flakes enabled
- (Optional) Docker daemon for loading/testing images locally

## Available Commands

### Build Docker Images

Build a Docker image for a specific architecture:

```bash
# Build x86_64 (amd64) image
nix build .#dockerImage-amd64

# Build ARM64 (aarch64) image - for Raspberry Pi 4
nix build .#dockerImage-aarch64
```

The resulting image will be available as `result` (a tarball that can be loaded into Docker).

### Build and Load into Docker

If you have Docker running locally, you can build and load in one command:

```bash
# Build and load amd64 image
nix run .#load-amd64

# Build and load aarch64 image
nix run .#load-aarch64
```

### Build All Images

Build all supported architectures at once:

```bash
nix run .#build-all-images
```

This creates:

- `result-amd64/` - x86_64 Docker image
- `result-aarch64/` - ARM64 Docker image

## Manual Docker Load

If you've built an image and want to load it manually:

```bash
# Build the image
nix build .#dockerImage-aarch64

# Load into Docker
docker load < result

# Verify it's loaded
docker images | grep fluxion-ha
```

## Architecture Support

| Architecture | Nix System | Docker Platform | Status |
|--------------|------------|-----------------|--------| | x86_64 (amd64) | `x86_64-linux` |
`linux/amd64` | ✅ Native build | | ARM64 (aarch64) | `aarch64-linux` | `linux/arm64` | ✅ |

## CI/CD Integration

You can integrate Nix builds into GitLab CI:

```yaml
build:aarch64-nix:
  stage: build
  image: nixos/nix:latest
  before_script:
    - nix --version
    - mkdir -p ~/.config/nix
    - echo "experimental-features = nix-command flakes" >> ~/.config/nix/nix.conf
  script:
    - cd fluxion
    - nix build .#dockerImage-aarch64
    - # Push to registry...
```

## Advantages Over Docker Build

### Traditional Docker Build

```bash
docker buildx build --platform linux/arm64 \
  --build-arg BUILD_FROM=ghcr.io/home-assistant/aarch64-base:3.18 \
  -f addon/Dockerfile -t fluxion-ha:aarch64 ..
```

**Issues:**

- Requires QEMU emulation (slow)
- Needs Docker daemon with buildx
- Less reproducible
- No caching between CI runs

### Nix Build

```bash
nix build .#dockerImage-aarch64
```

**Benefits:**

- Native cross-compilation (fast)
- No Docker daemon needed
- Bit-for-bit reproducible
- Efficient caching via Nix store
- Can build offline after first fetch

## Image Details

The Nix-built images include:

- FluxION main binary (`/bin/fluxion-main`)
- Minimal runtime dependencies (bash, coreutils)
- Home Assistant addon metadata labels

## Troubleshooting

### Build fails with "unknown experimental feature"

Enable flakes in your Nix configuration:

```bash
mkdir -p ~/.config/nix
echo "experimental-features = nix-command flakes" >> ~/.config/nix/nix.conf
```

### Cross-compilation fails

Make sure you have the necessary cross-compilation tools. On NixOS, this is handled automatically.
On non-NixOS systems, you may need to enable binfmt for foreign architectures.

### Image too large

The current implementation uses a minimal base. If you need to reduce size further, consider:

- Using `dockerTools.streamLayeredImage` instead of `buildImage`
- Stripping binaries more aggressively
- Removing unnecessary dependencies

## Next Steps

- Update `.gitlab-ci.yml` to use Nix builds instead of Docker builds
- Implement multi-arch manifest creation in Nix
