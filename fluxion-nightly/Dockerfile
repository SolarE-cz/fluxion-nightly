ARG TARGETARCH
ARG TARGETPLATFORM
# Map Docker's arch to Home Assistant's arch naming
# Docker uses: amd64, arm64
# Home Assistant uses: amd64, aarch64
ARG BUILD_ARCH=${TARGETARCH}
ARG BUILD_FROM=ghcr.io/home-assistant/${BUILD_ARCH}-base:3.18
FROM ${BUILD_FROM} AS builder-amd64

FROM ghcr.io/home-assistant/aarch64-base:3.18 AS builder-arm64

FROM builder-${TARGETARCH} AS builder

# Set shell
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# Install build dependencies and rustup
RUN apk add --no-cache \
    curl \
    git \
    openssl-dev \
    musl-dev \
    build-base \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable \
    && rm -rf /var/cache/apk/*

# Add Rust to PATH
ENV PATH="/root/.cargo/bin:${PATH}"

# Set working directory
WORKDIR /build

# Copy workspace configuration
COPY fluxion/Cargo.toml ./
COPY fluxion/rust-toolchain.toml ./
COPY fluxion/crates/ ./crates/

# Build release binary
RUN cargo build --release --bin fluxion

# Create final image stages
FROM ghcr.io/home-assistant/amd64-base:3.18 AS final-amd64

FROM ghcr.io/home-assistant/aarch64-base:3.18 AS final-arm64

FROM final-${TARGETARCH} AS final

# Install runtime dependencies
RUN apk add --no-cache \
    libgcc \
    && rm -rf /var/cache/apk/*

# Copy binary from builder
COPY --from=builder /build/target/release/fluxion /usr/local/bin/fluxion-main

# Copy run script
COPY fluxion/fluxion/run.sh /
RUN chmod a+x /run.sh

# Labels for Home Assistant addon
LABEL \
    io.hass.name="FluxION ECS" \
    io.hass.description="Home Assistant addon for PV plant automation with FluxION" \
    io.hass.version="0.2.21" \
    io.hass.type="addon" \
    io.hass.arch="amd64|aarch64"

CMD [ "/run.sh" ]
