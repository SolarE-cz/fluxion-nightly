{
  description = "FluxION-ECS - Solar Energy Management System";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    let
      # Supported architectures for cross-compilation
      supportedSystems = [ "x86_64-linux" "aarch64-linux" ];

      # Map Nix systems to HA base image architectures
      haBaseImages = {
        "x86_64-linux" = "ghcr.io/home-assistant/amd64-base:3.18";
        "aarch64-linux" = "ghcr.io/home-assistant/aarch64-base:3.18";
        "armv7l-linux" = "ghcr.io/home-assistant/armv7-base:3.18";
      };

      # Build fluxion for a specific target system
      buildFluxionFor = buildSystem: targetSystem:
        let
          overlays = [ (import rust-overlay) ];
          pkgs = import nixpkgs {
            system = buildSystem;
            inherit overlays;
            crossSystem =
              if buildSystem != targetSystem
              then nixpkgs.lib.systems.examples.${targetSystem} or { config = targetSystem; }
              else null;
          };

          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./fluxion/rust-toolchain.toml;

          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          };
        in
        rustPlatform.buildRustPackage {
          pname = "fluxion-main";
          version = "0.1.0";
          src = ./fluxion;
          cargoLock.lockFile = ./fluxion/Cargo.lock;

          nativeBuildInputs = with pkgs; [ pkg-config ];
          buildInputs = with pkgs; [ openssl ];

          # Build only the main binary
          cargoBuildFlags = [ "--bin" "fluxion" ];

          # Skip tests for cross-compilation
          doCheck = buildSystem == targetSystem;
        };

      # Build Docker image for a specific architecture
      buildDockerImageFor = buildSystem: targetSystem:
        let
          pkgs = import nixpkgs { system = buildSystem; };
          fluxionBinary = buildFluxionFor buildSystem targetSystem;
          arch =
            if targetSystem == "x86_64-linux" then "amd64"
            else if targetSystem == "aarch64-linux" then "aarch64"
            else if targetSystem == "armv7l-linux" then "armv7"
            else throw "Unsupported architecture: ${targetSystem}";
        in
        pkgs.dockerTools.buildImage {
          name = "fluxion-ha";
          tag = arch;

          # Use scratch as base - we'll reference HA base in documentation
          fromImage = null;

          copyToRoot = pkgs.buildEnv {
            name = "fluxion-root";
            paths = [
              fluxionBinary
              pkgs.bash
              pkgs.coreutils
            ];
            pathsToLink = [ "/bin" ];
          };

          config = {
            Cmd = [ "/bin/fluxion-main" ];
            Env = [
              "PATH=/bin"
            ];
            Labels = {
              "io.hass.name" = "FluxION ECS";
              "io.hass.description" = "Home Assistant addon for PV plant automation with FluxION";
              "io.hass.version" = "0.1.0";
              "io.hass.type" = "addon";
              "io.hass.arch" = arch;
            };
          };
        };

    in
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        # Use Rust version from toolchain file
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./fluxion/rust-toolchain.toml;

        # Add extensions for development
        rustDevToolchain = rustToolchain.override {
          extensions = [ "rust-src" "rust-analyzer" ];
        };

        # Import the treefmt-based formatter with addlicense
        formatter = import ./formatter.nix pkgs;

      in
      {
        # Define the formatter for `nix fmt`
        formatter = formatter;

        # Development shell
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustDevToolchain
            cargo-watch
            cargo-edit
            pkg-config
            openssl
          ];

          shellHook = ''
            echo "🚀 FluxION-ECS Development Environment"
            echo "   Rust: $(rustc --version)"
            echo "   Cargo: $(cargo --version)"
            echo ""
            echo "💡 Commands:"
            echo "   nix fmt              - Add license headers + format code"
            echo "   cargo build          - Build the project"
            echo "   cargo test           - Run tests"
            echo "   cargo clippy         - Run linter"
            echo ""
            echo "🐳 Docker builds:"
            echo "   nix build .#dockerImage-amd64    - Build x86_64 image"
            echo "   nix build .#dockerImage-aarch64  - Build ARM64 image"
            echo ""
            echo "   nix run .#load-amd64             - Build and load x86_64 image"
            echo "   nix run .#load-aarch64           - Build and load ARM64 image"
            echo ""
            echo "🧪 Testing:"
            echo "   nix run .#test-container         - Run in Docker (requires Docker)"
            echo "   nix run .#test-nixos-container   - Run in systemd-nspawn (NixOS)"
            echo "   nix run .#test-shell             - Interactive test shell"
            echo ""
            echo "📜 License: CC-BY-NC-ND-4.0 (SOLARE S.R.O.)"
          '';
        };

        # Native package build
        packages.default = buildFluxionFor system system;
        packages.fluxion = buildFluxionFor system system;

        # Docker images for different architectures
        packages.dockerImage-amd64 = buildDockerImageFor system "x86_64-linux";
        packages.dockerImage-aarch64 = buildDockerImageFor system "aarch64-linux";
        # Note: armv7 cross-compilation from x86_64 requires additional setup
        # packages.dockerImage-armv7 = buildDockerImageFor system "armv7l-linux";

        # Convenience apps to build and load images into Docker
        apps.load-amd64 = {
          type = "app";
          program = toString (pkgs.writeShellScript "load-amd64" ''
            echo "🐳 Building and loading amd64 Docker image..."
            IMAGE=$(nix build --no-link --print-out-paths .#dockerImage-amd64)
            echo "📦 Loading $IMAGE into Docker..."
            docker load < "$IMAGE"
            echo "✅ Image loaded as fluxion-ha:amd64"
          '');
        };

        apps.load-aarch64 = {
          type = "app";
          program = toString (pkgs.writeShellScript "load-aarch64" ''
            echo "🐳 Building and loading aarch64 Docker image..."
            IMAGE=$(nix build --no-link --print-out-paths .#dockerImage-aarch64)
            echo "📦 Loading $IMAGE into Docker..."
            docker load < "$IMAGE"
            echo "✅ Image loaded as fluxion-ha:aarch64"
          '');
        };

        # Build all docker images at once
        apps.build-all-images = {
          type = "app";
          program = toString (pkgs.writeShellScript "build-all" ''
            echo "🏗️  Building all Docker images..."
            echo ""
            echo "Building amd64..."
            nix build .#dockerImage-amd64 -o result-amd64
            echo "Building aarch64..."
            nix build .#dockerImage-aarch64 -o result-aarch64
            echo ""
            echo "✅ All images built:"
            echo "   result-amd64/"
            echo "   result-aarch64/"
          '');
        };

        # Run fluxion in a container for testing
        apps.test-container = {
          type = "app";
          program = toString (pkgs.writeShellScript "test-container" ''
            set -e
            
            # Check if Docker is available
            if ! command -v docker &> /dev/null; then
              echo "❌ Docker is not available"
              echo "Please install Docker or use 'nix run .#test-nixos-container' for NixOS containers"
              exit 1
            fi

            # Build the image
            echo "🔨 Building Docker image..."
            IMAGE=$(nix build --no-link --print-out-paths .#dockerImage-amd64)
            
            # Load into Docker
            echo "📦 Loading image into Docker..."
            docker load < "$IMAGE" | grep "Loaded image"
            
            # Run container
            echo "🚀 Starting test container..."
            echo ""
            echo "Container will be named: fluxion-test"
            echo "To stop: docker stop fluxion-test"
            echo "To view logs: docker logs -f fluxion-test"
            echo ""
            
            # Clean up existing test container if it exists
            docker rm -f fluxion-test 2>/dev/null || true
            
            # Run with common test settings
            docker run -d \
              --name fluxion-test \
              -p 8080:8080 \
              -e RUST_LOG=debug \
              fluxion-ha:amd64
            
            echo "✅ Container started!"
            echo ""
            echo "View logs:"
            docker logs -f fluxion-test
          '');
        };

        # Run in a NixOS container (no Docker daemon needed)
        apps.test-nixos-container = {
          type = "app";
          program = toString (pkgs.writeShellScript "test-nixos-container" ''
                        set -e
            
                        echo "🔨 Building fluxion binary..."
                        FLUXION=$(nix build --no-link --print-out-paths .#fluxion)
            
                        echo "🚀 Running fluxion in systemd-nspawn container..."
                        echo ""
                        echo "Press Ctrl+C to stop"
                        echo ""
            
                        # Create a minimal container root
                        CONTAINER_ROOT=$(mktemp -d)
                        trap "rm -rf $CONTAINER_ROOT" EXIT
            
                        # Set up minimal container filesystem
                        mkdir -p $CONTAINER_ROOT/{bin,etc,tmp,data}
            
                        # Copy fluxion binary and dependencies
                        ${pkgs.nix}/bin/nix-store -qR $FLUXION | ${pkgs.xargs}/bin/xargs -I {} cp -r {} $CONTAINER_ROOT/
            
                        # Create a wrapper script
                        cat > $CONTAINER_ROOT/bin/run-fluxion <<EOF
            #!/bin/sh
            export RUST_LOG=debug
            exec $FLUXION/bin/fluxion-main
            EOF
                        chmod +x $CONTAINER_ROOT/bin/run-fluxion
            
                        echo "Container root: $CONTAINER_ROOT"
                        echo ""
            
                        # Run in namespace (requires systemd-nspawn or unshare)
                        if command -v systemd-nspawn &> /dev/null; then
                          sudo systemd-nspawn \
                            --directory=$CONTAINER_ROOT \
                            --bind=/tmp:/host-tmp \
                            --setenv=RUST_LOG=debug \
                            /bin/run-fluxion
                        else
                          echo "⚠️  systemd-nspawn not available, running directly:"
                          RUST_LOG=debug $FLUXION/bin/fluxion-main
                        fi
          '');
        };

        # Interactive shell with fluxion available for testing
        apps.test-shell = {
          type = "app";
          program = toString (pkgs.writeShellScript "test-shell" ''
            echo "🔨 Building fluxion..."
            FLUXION=$(nix build --no-link --print-out-paths .#fluxion)
            
            echo "🚀 Starting test shell..."
            echo ""
            echo "Fluxion binary available at: $FLUXION/bin/fluxion-main"
            echo ""
            echo "Quick commands:"
            echo "  run-fluxion        - Run fluxion with debug logging"
            echo "  fluxion-bin        - Path to fluxion binary"
            echo ""
            
            export FLUXION_BIN=$FLUXION/bin/fluxion-main
            export RUST_LOG=debug
            
            # Create helper function
            run_fluxion() {
              echo "Running fluxion..."
              $FLUXION_BIN "$@"
            }
            export -f run_fluxion
            
            ${pkgs.bashInteractive}/bin/bash --rcfile <(echo '
              alias run-fluxion="run_fluxion"
              alias fluxion-bin="echo $FLUXION_BIN"
              PS1="\[\033[1;32m\][fluxion-test]\[\033[0m\] \w $ "
              echo "Type 'run-fluxion' to start, or 'exit' to quit"
            ')
          '');
        };
      }
    );
}
