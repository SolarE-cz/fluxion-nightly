ARG BUILD_FROM=ghcr.io/home-assistant/amd64-base:3.18
FROM ${BUILD_FROM}

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
COPY Cargo.toml ./
COPY rust-toolchain.toml ./
COPY crates/ ./crates/

# Build release binary
RUN cargo build --release --bin fluxion

# Create final image
FROM ${BUILD_FROM}

# Install runtime dependencies
RUN apk add --no-cache \
    libgcc \
    && rm -rf /var/cache/apk/*

# Copy binary from builder
COPY --from=0 /build/target/release/fluxion /usr/local/bin/fluxion-main

# Copy run script
COPY run.sh /
RUN chmod a+x /run.sh

# Labels for Home Assistant addon
LABEL \
    io.hass.name="FluxION ECS" \
    io.hass.description="Home Assistant addon for PV plant automation with FluxION" \
    io.hass.version="0.1.18" \
    io.hass.type="addon" \
    io.hass.arch="amd64|aarch64"

CMD [ "/run.sh" ]
