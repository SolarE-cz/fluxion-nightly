{
  description = "FluxION-ECS - Solar Energy Management System";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, crane, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        # Import nixpkgs with rust-overlay
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
          config = {
            android_sdk.accept_license = true;
            # Allow unfree packages (required for Android SDK)
            allowUnfree = true;
          };
        };

        # Create crane library instance
        craneLib = crane.mkLib pkgs;

        # Use Rust version from toolchain file
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./fluxion/rust-toolchain.toml;

        # Override crane library with our toolchain
        craneLibToolchain = craneLib.overrideToolchain rustToolchain;

        # Android SDK for mobile development
        androidComposition = pkgs.androidenv.composeAndroidPackages {
          platformVersions = [ "36" ];
          buildToolsVersions = [ "35.0.0" ];
          includeNDK = true;
          ndkVersions = [ "26.3.11579264" ];
          abiVersions = [ "armeabi-v7a" "arm64-v8a" "x86" "x86_64" ];
          extraLicenses = [
            "android-sdk-preview-license"
            "android-googletv-license"
            "android-sdk-arm-dbt-license"
            "google-gdk-license"
            "intel-android-extra-license"
            "intel-android-sysimage-license"
            "mips-android-sysimage-license"
          ];
        };
        androidSdk = androidComposition.androidsdk;

        # Rust toolchain with Android cross-compilation targets for mobile builds
        rustToolchainMobile = (pkgs.rust-bin.fromRustupToolchainFile ./fluxion/rust-toolchain.toml).override {
          targets = [
            "aarch64-linux-android"
            "armv7-linux-androideabi"
            "i686-linux-android"
            "x86_64-linux-android"
          ];
        };

        # Custom source filter to include .ftl locale files, .html templates, and .py scripts
        sourceFilter = path: type:
          let
            baseName = baseNameOf path;
            ext = pkgs.lib.last (pkgs.lib.splitString "." baseName);
          in
          # Include standard Rust files
          (craneLib.filterCargoSources path type) ||
          # Include .ftl files for i18n
          (ext == "ftl") ||
          # Include .html files for web templates
          (ext == "html") ||
          # Include .py files for scripts
          (ext == "py");

        # Common arguments for all crane builds
        commonArgs = {
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = sourceFilter;
          };
          strictDeps = true;
          doCheck = false; # We'll control testing separately
          buildInputs = [ ];
          nativeBuildInputs = [ ];
          # Explicitly set paths to Cargo files
          cargoToml = ./fluxion/Cargo.toml;
          cargoLock = ./fluxion/Cargo.lock;
          # Set the working directory to the fluxion subdirectory where Cargo.toml is
          postUnpack = ''
            sourceRoot="$sourceRoot/fluxion"
          '';
        };

        # Build dependencies only once for reuse
        cargoArtifacts = craneLibToolchain.buildDepsOnly (commonArgs // {
          pname = "fluxion-deps";
        });

        # Build the main fluxion binary
        fluxion-main = craneLibToolchain.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          pname = "fluxion-main";
          cargoBuildFlags = [ "--bin" "fluxion" ];
        });

        # Build the version binary
        fluxion-version = craneLibToolchain.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          pname = "fluxion-version";
          cargoBuildFlags = [ "--bin" "fluxion-version" ];
        });

        # Build the fluxion-server binary
        fluxion-server-bin = craneLibToolchain.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          pname = "fluxion-server";
          cargoBuildFlags = [ "--bin" "fluxion-server" ];
        });

        # Clippy package using crane
        fluxion-clippy = craneLibToolchain.cargoClippy (commonArgs // {
          inherit cargoArtifacts;
          cargoClippyExtraArgs = "--all-targets -- --deny warnings";
        });

        # Audit package using crane
        fluxion-audit = craneLibToolchain.cargoAudit (commonArgs // {
          inherit cargoArtifacts;
          advisory-db = pkgs.fetchFromGitHub {
            owner = "RustSec";
            repo = "advisory-db";
            # Lock to specific commit from December 8, 2025
            rev = "6b4a28c7201255e36850fd6e2ca1fbdd8d4f5b4d";
            hash = "sha256-A6TExffYU4WElf8GW8yaqUFK2P00ES/a/HSqqhIL7pk=";
          };
          cargoAuditExtraArgs = "";
        });

        # Test package using crane
        fluxion-tests = craneLibToolchain.cargoNextest (commonArgs // {
          inherit cargoArtifacts;
        });

        # Import the treefmt-based formatter with addlicense
        formatter = import ./formatter.nix { inherit pkgs rustToolchain; };

        # GitHub publish script as a proper Nix package
        github-publish-script = pkgs.writeScriptBin "publish-github" ''
          #!${pkgs.python3.withPackages (ps: with ps; [ pyyaml tomli tomli-w ])}/bin/python3
          ${builtins.readFile ./scripts/publish_github.py}
        '';

        # Supported architectures for cross-compilation
        supportedSystems = [ "x86_64-linux" "aarch64-linux" ];

        # Map Nix systems to HA base image architectures
        haBaseImages = {
          "x86_64-linux" = "ghcr.io/home-assistant/amd64-base:3.18";
          "aarch64-linux" = "ghcr.io/home-assistant/aarch64-base:3.18";
        };

        # Build Docker image for a specific architecture
        buildDockerImageFor = targetSystem:
          let
            fluxionBinary =
              if system == targetSystem then
                fluxion-main
              else
              # Cross-compilation case
                let
                  crossPkgs = import nixpkgs {
                    inherit system;
                    overlays = [ (import rust-overlay) ];
                    crossSystem = nixpkgs.lib.systems.examples.${targetSystem} or { config = targetSystem; };
                  };
                  crossCraneLib = crane.mkLib crossPkgs;
                  crossCraneLibToolchain = crossCraneLib.overrideToolchain rustToolchain;
                in
                crossCraneLibToolchain.buildPackage (commonArgs // {
                  pname = "fluxion-main";
                  cargoBuildFlags = [ "--bin" "fluxion" ];
                  doCheck = false;
                });

            arch =
              if targetSystem == "x86_64-linux" then "amd64"
              else if targetSystem == "aarch64-linux" then "aarch64"
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
      {
        # Define the formatter for `nix fmt`
        formatter = formatter;

        # Development shell
        devShells.default = craneLibToolchain.devShell {
          packages = with pkgs; [
            cargo-watch
            cargo-edit
            pkg-config
            openssl
            # Tauri 2.0 dependencies (for fluxion-mobile development)
            gtk3
            webkitgtk_4_1
            libsoup_3
            # Arti Tor client dependency (rusqlite needs sqlite3 at link time)
            sqlite
          ];

          # Environment variables for development
          MY_CUSTOM_DEV_URL = "http://localhost:3000";

          # Shell hook for development environment
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
            echo "🔧 CI/CD Apps (for pipeline jobs):"
            echo "   nix build .#fluxion-tests        - Run unit tests"
            echo "   nix build .#fluxion-clippy       - Run clippy linter"
            echo "   nix run .#ci-format-check        - Check code formatting"
            echo "   nix build .#fluxion-audit        - Run security audit"
            echo "   nix run .#ci-docker-build-amd64  - Build Docker AMD64 image"
            echo "   nix run .#ci-publish-dry-run     - Preview files for publish (dry-run)"
            echo "   nix run .#ci-publish-github      - Publish to GitHub"
            echo "   nix run .#ci-bump-version        - Bump version (set BUMP_TYPE=major|minor|patch)"
            echo ""
            echo "📜 License: CC-BY-NC-ND-4.0 (SOLARE S.R.O.)"
          '';
        };

        # Data analysis shell (Python + matplotlib/pandas/seaborn)
        devShells.analysis = pkgs.mkShell {
          packages = with pkgs; [
            (python3.withPackages (ps: with ps; [
              matplotlib
              numpy
              pandas
              seaborn
              scipy
            ]))
          ];

          shellHook = ''
            echo "FluxION Analysis Environment"
            echo "  Python: $(python3 --version)"
            echo ""
            echo "Available packages: matplotlib, numpy, pandas, seaborn, scipy"
            echo ""
            echo "Usage:"
            echo "  python3 ralph/scripts/c10_analysis.py    - Run C10 sweep analysis"
          '';
        };

        # Mobile (Android) development shell
        devShells.mobile = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchainMobile
            cargo-watch
            cargo-edit
            pkg-config
            openssl
            sqlite
            # Tauri 2.0 desktop dependencies (needed for cargo check on host)
            gtk3
            webkitgtk_4_1
            libsoup_3
            # Tauri CLI (provides `cargo tauri` subcommand)
            cargo-tauri
            # Android development
            androidSdk
            jdk17
            gradle
          ];

          ANDROID_HOME = "${androidSdk}/libexec/android-sdk";
          ANDROID_SDK_ROOT = "${androidSdk}/libexec/android-sdk";
          ANDROID_NDK_HOME = "${androidSdk}/libexec/android-sdk/ndk-bundle";
          NDK_HOME = "${androidSdk}/libexec/android-sdk/ndk-bundle";
          JAVA_HOME = "${pkgs.jdk17}/lib/openjdk";
          GRADLE_OPTS = "-Dorg.gradle.project.android.aapt2FromMavenOverride=${androidSdk}/libexec/android-sdk/build-tools/35.0.0/aapt2";

          shellHook = ''
            echo "FluxION Mobile Android Development Environment"
            echo "  Rust: $(rustc --version)"
            echo "  Android SDK: $ANDROID_HOME"
            echo "  NDK: $ANDROID_NDK_HOME"
            echo "  JDK: $(java -version 2>&1 | head -1)"
            echo ""
            echo "Commands:"
            echo "  cd fluxion-mobile && cargo tauri android init    - Initialize Android project"
            echo "  cd fluxion-mobile && cargo tauri android build --debug  - Build debug APK"
            echo "  cd fluxion-mobile && cargo tauri android build   - Build release APK"
          '';
        };

        # Packages
        packages = {
          default = fluxion-main;
          fluxion = fluxion-main;
          fluxion-version = fluxion-version;
          fluxion-clippy = fluxion-clippy;
          fluxion-audit = fluxion-audit;
          fluxion-tests = fluxion-tests;
          fluxion-server = fluxion-server-bin;

          # CI/CD tools
          github-publish-script = github-publish-script;

          # Docker images for different architectures
          dockerImage-amd64 = buildDockerImageFor "x86_64-linux";
          dockerImage-aarch64 = buildDockerImageFor "aarch64-linux";
        };

        # Apps
        apps = {
          # Convenience apps to build and load images into Docker
          load-amd64 = {
            type = "app";
            program = toString (pkgs.writeShellScript "load-amd64" ''
              echo "🐳 Building and loading amd64 Docker image..."
              IMAGE=$(nix build --no-link --print-out-paths .#dockerImage-amd64)
              echo "📦 Loading $IMAGE into Docker..."
              docker load < "$IMAGE"
              echo "✅ Image loaded as fluxion-ha:amd64"
            '');
          };

          load-aarch64 = {
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
          build-all-images = {
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
          test-container = {
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
          test-nixos-container = {
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
                          ${pkgs.nix}/bin/nix-store -qR $FLUXION | ${pkgs.findutils}/bin/xargs -I {} cp -r {} $CONTAINER_ROOT/

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

          # Version app - outputs the workspace version from Cargo.toml
          # This is the single source of truth for versioning
          version = {
            type = "app";
            program = toString (pkgs.writeShellScript "fluxion-version" ''
              ${fluxion-version}/bin/fluxion-version
            '');
          };

          # Interactive shell with fluxion available for testing
          test-shell = {
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

          # ============= Deployment Apps =============

          # Deploy fluxion-server to remote NixOS host
          deploy-server = {
            type = "app";
            program = toString (pkgs.writeShellScript "deploy-server" ''
              set -euo pipefail

              export PATH="${pkgs.openssh}/bin:${pkgs.coreutils}/bin:${pkgs.nix}/bin:$PATH"

              SERVER="tever@reborn"
              REMOTE_DIR="/home/tever/fluxion"
              CONFIG_SRC="fluxion/crates/fluxion-server/config.example.toml"

              echo "Building fluxion-server..."
              RESULT=$(nix build --no-link --print-out-paths .#fluxion-server)
              echo "Built: $RESULT"

              echo ""
              echo "Copying closure to remote Nix store..."
              nix copy --to "ssh://$SERVER" "$RESULT"

              echo ""
              echo "Updating symlink and restarting service..."
              ssh "$SERVER" "\
                sudo mkdir -p $REMOTE_DIR/data && \
                sudo ln -sfn $RESULT/bin/fluxion-server $REMOTE_DIR/fluxion-server && \
                if [ ! -f $REMOTE_DIR/config.toml ]; then \
                  echo 'No config.toml found — copying example config (edit before starting!)'; \
                  sudo cp $REMOTE_DIR/config.example.toml $REMOTE_DIR/config.toml 2>/dev/null || true; \
                fi && \
                sudo systemctl restart fluxion-server"

              echo ""
              echo "Deployed! Current status:"
              ssh "$SERVER" "sudo systemctl status fluxion-server --no-pager" || true
            '');
          };

          # ============= CI/CD Pipeline Apps =============

          # Check code formatting and license headers
          ci-format-check = {
            type = "app";
            program = toString (pkgs.writeShellScript "ci-format-check" ''
              set -e
              echo "🔍 Checking code formatting and license headers..."
              if ! ${formatter}/bin/fluxion-fmt --fail-on-change; then
                echo "❌ Code is not properly formatted or missing license headers!"
                echo ""
                echo "To fix this, run locally:"
                echo "  nix fmt"
                echo ""
                echo "This will:"
                echo "  - Add CC-BY-NC-ND-4.0 license headers to files missing them"
                echo "  - Format Rust code with rustfmt"
                echo "  - Format Nix, TOML, and other files"
                echo ""
                echo "Then commit the changes."
                exit 1
              fi
              echo "✅ All files are properly formatted with correct license headers!"
            '');
          };

          # Run security audit
          ci-security-audit = {
            type = "app";
            program = toString (pkgs.writeShellScript "ci-security-audit" ''
              set -e
              echo "🔒 Running security audit..."
              ${pkgs.nix}/bin/nix build .#fluxion-audit
              echo "✅ Security audit completed"
            '');
          };

          # Build Docker AMD64 image (test build)
          ci-docker-build-amd64 = {
            type = "app";
            program = toString (pkgs.writeShellScript "ci-docker-build-amd64" ''
              set -e
              echo "🐳 Building Docker AMD64 image with Nix..."
              ${pkgs.nix}/bin/nix build .#dockerImage-amd64
              echo "✅ Docker AMD64 image built successfully"
            '');
          };

          # Dry-run publish to preview files without actually publishing
          ci-publish-dry-run = {
            type = "app";
            program = toString (pkgs.writeShellScript "ci-publish-dry-run" ''
              set -euo pipefail

              # Add required tools to PATH
              export PATH="${pkgs.python3.withPackages (ps: with ps; [ pyyaml tomli tomli-w ])}/bin:${pkgs.tree}/bin:${pkgs.coreutils}/bin:${pkgs.findutils}/bin:${pkgs.gnugrep}/bin:${pkgs.bash}/bin:$PATH"

              # Create temporary work directory
              WORK_DIR=$(mktemp -d)
              trap "rm -rf $WORK_DIR" EXIT

              echo "🔍 FluxION Publish Dry-Run"
              echo "=========================="
              echo "Work directory: $WORK_DIR"
              echo ""

              # Run the GitHub publish script in dry-run mode
              echo "📋 Building snapshot from manifest..."
              ${github-publish-script}/bin/publish-github --dry-run --output "$WORK_DIR/snapshot"

              cd "$WORK_DIR/snapshot"

              # Count total files and directories
              TOTAL_FILES=$(find . -type f | wc -l)
              TOTAL_DIRS=$(find . -type d | wc -l)

              echo ""
              echo "📊 Summary:"
              echo "- Total files: $TOTAL_FILES"
              echo "- Total directories: $TOTAL_DIRS"
              echo ""

              echo "🗂️ Directory tree:"
              tree . 2>/dev/null || find . -type d | sort
              echo ""

              echo "📝 All files with sizes:"
              find . -type f -exec ls -lah {} + | sort -k9
              echo ""

              # Check for locale files specifically
              echo "🌍 Locale files check:"
              if find . -name "*.ftl" -type f | head -1 >/dev/null; then
                echo "✅ Found locale files:"
                find . -name "*.ftl" -type f | sort
              else
                echo "❌ No .ftl locale files found!"
              fi
              echo ""

              echo "✅ Dry-run complete. Files listed above would be published."
            '');
          };

          # Publish to GitHub (manual job)
          ci-publish-github = {
            type = "app";
            program = toString (pkgs.writeShellScript "ci-publish-github" ''
              set -euo pipefail

              # Add git, SSH and core utilities to PATH
              export PATH="${pkgs.git}/bin:${pkgs.openssh}/bin:${pkgs.coreutils}/bin:${pkgs.findutils}/bin:${pkgs.gnugrep}/bin:${pkgs.bash}/bin:$PATH"

              # Run the GitHub publish script
              ${github-publish-script}/bin/publish-github
            '');
          };

          # Bump version after GitHub publish
          ci-bump-version = {
            type = "app";
            program = toString (pkgs.writeShellScript "ci-bump-version" ''
              set -euo pipefail

              # Add Python packages, Rust toolchain and tools to PATH
              export PATH="${pkgs.python3.withPackages (ps: with ps; [ tomli tomli-w ])}/bin:${rustToolchain}/bin:${pkgs.git}/bin:${pkgs.bash}/bin:${pkgs.gnused}/bin:${pkgs.gawk}/bin:${pkgs.gnugrep}/bin:${pkgs.coreutils}/bin:$PATH"

              # Colors for output
              RED='\033[0;31m'
              GREEN='\033[0;32m'
              YELLOW='\033[1;33m'
              BLUE='\033[0;34m'
              NC='\033[0m' # No Color

              info() {
                echo -e "''${GREEN}[INFO]''${NC} $1"
              }

              warn() {
                echo -e "''${YELLOW}[WARN]''${NC} $1"
              }

              error() {
                echo -e "''${RED}[ERROR]''${NC} $1"
                exit 1
              }

              step() {
                echo -e "''${BLUE}[STEP]''${NC} $1"
              }

              BUMP_TYPE="''${BUMP_TYPE:-patch}"
              PROJECT_ROOT="$(pwd)/fluxion"
              CARGO_TOML="$PROJECT_ROOT/Cargo.toml"
              ADDON_CONFIG="$PROJECT_ROOT/fluxion/config.yaml"
              DOCKERFILE="$PROJECT_ROOT/Dockerfile"
              NIGHTLY_MANIFEST="$PROJECT_ROOT/release-manifests/nightly.yml"

              echo "📈 Bumping version..."
              echo "Bump type - $BUMP_TYPE"

              # Check if Cargo.toml exists
              if [[ ! -f $CARGO_TOML ]]; then
                error "Cargo.toml not found at $CARGO_TOML"
              fi

              # Read current version from Cargo.toml
              CURRENT_VERSION=$(grep -m1 '^version = ' "$CARGO_TOML" | sed 's/version = "//;s/"//')
              info "Current version: $CURRENT_VERSION"

              # Parse version components
              if [[ ! $CURRENT_VERSION =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
                error "Invalid version format in Cargo.toml. Expected semver format (e.g., 0.1.9)"
              fi

              MAJOR="''${BASH_REMATCH[1]}"
              MINOR="''${BASH_REMATCH[2]}"
              PATCH="''${BASH_REMATCH[3]}"

              # Calculate new version based on bump type
              case "$BUMP_TYPE" in
              major)
                MAJOR=$((MAJOR + 1))
                MINOR=0
                PATCH=0
                ;;
              minor)
                MINOR=$((MINOR + 1))
                PATCH=0
                ;;
              patch | hotfix)
                PATCH=$((PATCH + 1))
                ;;
              *)
                error "Invalid bump type: $BUMP_TYPE. Use 'major', 'minor', or 'patch'/'hotfix'"
                ;;
              esac

              NEW_VERSION="$MAJOR.$MINOR.$PATCH"
              info "New version: $NEW_VERSION"

              # Update Cargo.toml (single source of truth)
              step "Updating Cargo.toml..."
              sed -i "s/^version = \".*\"/version = \"$NEW_VERSION\"/" "$CARGO_TOML"

              # Verify the change
              UPDATED_VERSION=$(grep -m1 '^version = ' "$CARGO_TOML" | sed 's/version = "//;s/"//')
              if [[ $UPDATED_VERSION != "$NEW_VERSION" ]]; then
                error "Failed to update Cargo.toml. Expected $NEW_VERSION, got $UPDATED_VERSION"
              fi
              info "✓ Cargo.toml updated to $NEW_VERSION"

              # Update Cargo.lock
              step "Updating Cargo.lock..."
              (cd "$PROJECT_ROOT" && cargo update --workspace --quiet) || {
                warn "cargo update failed, attempting fallback with cargo check"
                (cd "$PROJECT_ROOT" && cargo check --quiet) || true
              }
              info "✓ Cargo.lock updated"

              # Sync all derived version sources
              step "Syncing derived version sources..."
              info "Source of truth: Cargo.toml version = $NEW_VERSION"

              # Update config.yaml
              if [[ -f $ADDON_CONFIG ]]; then
                info "Updating $ADDON_CONFIG..."
                sed -i "s/^version: \".*\"/version: \"$NEW_VERSION\"/" "$ADDON_CONFIG"

                # Verify
                UPDATED=$(grep -m1 '^version:' "$ADDON_CONFIG" | sed 's/version: "//;s/"//')
                if [[ $UPDATED == "$NEW_VERSION" ]]; then
                  info "  ✓ config.yaml updated to $NEW_VERSION"
                else
                  error "  ✗ Failed to update config.yaml (got $UPDATED)"
                fi
              else
                warn "config.yaml not found at $ADDON_CONFIG, skipping"
              fi

              # Update Dockerfile
              if [[ -f $DOCKERFILE ]]; then
                info "Updating $DOCKERFILE..."
                sed -i "s/io\.hass\.version=\"[^\"]*\"/io.hass.version=\"$NEW_VERSION\"/" "$DOCKERFILE"

                # Verify
                UPDATED=$(grep 'io\.hass\.version=' "$DOCKERFILE" | sed 's/.*io\.hass\.version="\([^"]*\)".*/\1/')
                if [[ $UPDATED == "$NEW_VERSION" ]]; then
                  info "  ✓ Dockerfile updated to $NEW_VERSION"
                else
                  error "  ✗ Failed to update Dockerfile (got $UPDATED)"
                fi
              else
                warn "Dockerfile not found at $DOCKERFILE, skipping"
              fi

              # Update nightly.yml manifest version mutation
              if [[ -f $NIGHTLY_MANIFEST ]]; then
                info "Updating $NIGHTLY_MANIFEST version mutation..."
                sed -i "/path: version/,/value:/ s/value: \".*\"/value: \"$NEW_VERSION\"/" "$NIGHTLY_MANIFEST"

                # Verify
                UPDATED=$(grep -A1 "path: version" "$NIGHTLY_MANIFEST" | grep "value:" | sed 's/.*value: "//;s/".*//')
                if [[ $UPDATED == "$NEW_VERSION" ]]; then
                  info "  ✓ nightly.yml version mutation updated to $NEW_VERSION"
                else
                  error "  ✗ Failed to update nightly.yml version mutation (got $UPDATED)"
                fi
              else
                warn "nightly.yml not found at $NIGHTLY_MANIFEST, skipping"
              fi

              info "✅ All derived version sources synced to $NEW_VERSION"

              echo ""
              info "✅ Version bumped successfully: $CURRENT_VERSION → $NEW_VERSION"
              echo ""
              echo "Files updated:"
              echo "  - fluxion/Cargo.toml"
              echo "  - fluxion/Cargo.lock"
              echo "  - fluxion/fluxion/config.yaml"
              echo "  - fluxion/Dockerfile"
              echo "  - fluxion/release-manifests/nightly.yml"
              echo ""
              echo "Next steps:"
              echo "  1. Review changes: git diff"
              echo "  2. Commit all changes with: git add -A && git commit -m 'Bump version to $NEW_VERSION'"
              echo "  3. Push: git push origin main"
            '');
          };
        };

        # Checks for CI
        checks = {
          # Build the crate as part of `nix flake check` for convenience
          package = fluxion-main;

          # Run clippy in checks
          clippy = fluxion-clippy;

          # Run tests in checks
          tests = fluxion-tests;
        };
      }
    );
}
