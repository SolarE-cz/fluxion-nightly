{
  description = "FluxION ECS - Energy Control System for PV plant automation";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        python = pkgs.python3.withPackages (ps: [ ps.pathspec ]);

        # Read Rust version from rust-toolchain.toml
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./fluxion/rust-toolchain.toml;

        # Build the Rust binary using the toolchain from rust-toolchain.toml
        fluxion-binary = (pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        }).buildRustPackage {
          pname = "fluxion";
          version = "0.1.0";

          src = ./fluxion;

          cargoLock = {
            lockFile = ./fluxion/Cargo.lock;
          };

          nativeBuildInputs = with pkgs; [
            pkg-config
          ];

          buildInputs = with pkgs; [
            openssl
          ];

          # Run tests during build
          doCheck = true;

          meta = with pkgs.lib; {
            description = "FluxION ECS - Energy Control System for PV plant automation";
            homepage = "https://github.com/your-org/fluxion";
            license = licenses.mit;
            mainProgram = "fluxion-main";
          };
        };

        # Build the Home Assistant addon Docker image
        ha-addon-image = pkgs.dockerTools.buildLayeredImage {
          name = "fluxion-ha-addon";
          tag = "latest";

          contents = [
            fluxion-binary
            pkgs.bashInteractive
            pkgs.coreutils
          ];

          config = {
            Cmd = [ "${fluxion-binary}/bin/fluxion-main" ];
            Env = [
              "PATH=/bin"
            ];
            Labels = {
              "io.hass.name" = "FluxION ECS";
              "io.hass.description" = "Energy Control System for PV plant automation";
              "io.hass.type" = "addon";
              "io.hass.version" = "0.1.0";
            };
          };
        };

      in
      {
        packages = {
          # Default: the Rust binary
          default = fluxion-binary;

          # The Rust binary
          fluxion = fluxion-binary;

          # Home Assistant addon as Docker image
          ha-addon = ha-addon-image;

          # Addon configuration bundle
          ha-addon-bundle = pkgs.stdenv.mkDerivation {
            name = "fluxion-ha-addon-bundle";
            version = "0.1.0";

            src = ./fluxion/addon;

            installPhase = ''
              mkdir -p $out
              cp -r * $out/
              
              # Add the binary
              mkdir -p $out/bin
              cp ${fluxion-binary}/bin/fluxion-main $out/bin/
            '';

            meta = {
              description = "FluxION ECS Home Assistant addon bundle";
            };
          };
        };

        apps = {
          # Run the binary directly
          default = {
            type = "app";
            program = "${fluxion-binary}/bin/fluxion-main";
          };

          # Run the binary
          fluxion = {
            type = "app";
            program = "${fluxion-binary}/bin/fluxion-main";
          };

          # Test the addon in Docker
          test-ha-addon = {
            type = "app";
            program = toString (pkgs.writeShellScript "test-ha-addon" ''
              set -e
              echo "🐳 Building and testing Home Assistant addon..."
              
              # Load the image
              ${pkgs.docker}/bin/docker load < ${ha-addon-image}
              
              echo "✅ Docker image loaded: fluxion-ha-addon:latest"
              echo ""
              echo "To run the addon locally:"
              echo "  docker run --rm -e DEBUG_MODE=true fluxion-ha-addon:latest"
              echo ""
              echo "To inspect the image:"
              echo "  docker run --rm -it --entrypoint=/bin/bash fluxion-ha-addon:latest"
            '');
          };

          # Run all tests
          test-all = {
            type = "app";
            program = toString (pkgs.writeShellScript "test-all" ''
              set -e
              echo "🧪 Running all FluxION ECS tests..."
              echo ""

              cd ${./fluxion}

              echo "1️⃣  Running unit tests..."
              ${pkgs.cargo}/bin/cargo test --workspace
              echo ""
              
              echo "2️⃣  Running clippy..."
              ${pkgs.cargo}/bin/cargo clippy --workspace -- -D warnings
              echo ""
              
              echo "3️⃣  Checking formatting..."
              ${pkgs.cargo}/bin/cargo fmt -- --check
              echo ""
              
              echo "✅ All tests passed!"
            '');
          };

          # Build the addon for distribution
          build-addon = {
            type = "app";
            program = toString (pkgs.writeShellScript "build-addon" ''
              set -e
              echo "📦 Building FluxION ECS Home Assistant addon..."
              
              # Build the Docker image
              echo "Building Docker image..."
              nix build .#ha-addon
              
              echo ""
              echo "✅ Addon built successfully!"
              echo ""
              echo "Docker image saved to: result"
              echo ""
              echo "To load the image:"
              echo "  docker load < result"
            '');
          };
        };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            # Rust toolchain from rust-toolchain.toml
            rustToolchain
            cargo-audit
            cargo-watch

            # Build dependencies
            pkg-config
            openssl

            # Docker for testing
            docker
            docker-compose

            # Utilities
            jq
            yq

            # AI helpers
            firejail
            python
          ];

          shellHook = ''
            # AI helper setup
            export ORIGINAL_CLAUDE=$(command -v claude 2>/dev/null || echo "claude not found")
            export ORIGINAL_GEMINI=$(command -v gemini 2>/dev/null || echo "gemini not found")

            mkdir -p .nix-bin

            cat > .nix-bin/claude <<'EOF'
            #!${python}/bin/python
            import sys
            import os
            import subprocess
            import tempfile
            import pathspec
            from pathlib import Path

            def main(args):
                cwd = os.getcwd()
                aiignore_path = Path('.aiignore')
                if not aiignore_path.exists():
                    original = os.environ.get('ORIGINAL_CLAUDE', 'claude')
                    subprocess.check_call([original] + args)
                    return

                with open(aiignore_path) as f:
                    lines = f.read().splitlines()

                spec = pathspec.PathSpec.from_lines('gitignore', lines)

                hidden_dirs = []
                hidden_files = []

                for root, dirs, files in os.walk('.', topdown=True):
                    rel_root = os.path.relpath(root, '.')
                    for d in list(dirs):
                        rel = os.path.join(rel_root, d)
                        if spec.match_file(rel + '/'):
                            full = os.path.join(cwd, rel)
                            hidden_dirs.append(full)
                            dirs.remove(d)
                    for f in files:
                        rel = os.path.join(rel_root, f)
                        if spec.match_file(rel):
                            full = os.path.join(cwd, rel)
                            hidden_files.append(full)

                empty_dir = tempfile.mkdtemp()
                empty_file_dir = tempfile.mkdtemp()
                empty_file = os.path.join(empty_file_dir, 'empty')
                with open(empty_file, 'w') as ef:
                    pass

                firejail_args = [
                    'firejail',
                    '--noprofile',
                    '--quiet',
                    '--chdir', cwd,
                ]

                for d in hidden_dirs:
                    firejail_args += ['--bind', f'{empty_dir},{d}']
                for f in hidden_files:
                    firejail_args += ['--bind', f'{empty_file},{f}']

                original = os.environ.get('ORIGINAL_CLAUDE', 'claude')
                firejail_args += ['--', original] + args

                try:
                    subprocess.check_call(firejail_args)
                finally:
                    try:
                        os.rmdir(empty_dir)
                        os.unlink(empty_file)
                        os.rmdir(empty_file_dir)
                    except:
                        pass

            if __name__ == '__main__':
                main(sys.argv[1:])
            EOF

            chmod +x .nix-bin/claude

            cat > .nix-bin/gemini <<'EOF'
            #!${python}/bin/python
            import sys
            import os
            import subprocess
            import tempfile
            import pathspec
            from pathlib import Path

            def main(args):
                cwd = os.getcwd()
                aiignore_path = Path('.aiignore')
                if not aiignore_path.exists():
                    original = os.environ.get('ORIGINAL_GEMINI', 'gemini')
                    subprocess.check_call([original] + args)
                    return

                with open(aiignore_path) as f:
                    lines = f.read().splitlines()

                spec = pathspec.PathSpec.from_lines('gitignore', lines)

                hidden_dirs = []
                hidden_files = []

                for root, dirs, files in os.walk('.', topdown=True):
                    rel_root = os.path.relpath(root, '.')
                    for d in list(dirs):
                        rel = os.path.join(rel_root, d)
                        if spec.match_file(rel + '/'):
                            full = os.path.join(cwd, rel)
                            hidden_dirs.append(full)
                            dirs.remove(d)
                    for f in files:
                        rel = os.path.join(rel_root, f)
                        if spec.match_file(rel):
                            full = os.path.join(cwd, rel)
                            hidden_files.append(full)

                empty_dir = tempfile.mkdtemp()
                empty_file_dir = tempfile.mkdtemp()
                empty_file = os.path.join(empty_file_dir, 'empty')
                with open(empty_file, 'w') as ef:
                    pass

                firejail_args = [
                    'firejail',
                    '--noprofile',
                    '--quiet',
                    '--chdir', cwd,
                ]

                for d in hidden_dirs:
                    firejail_args += ['--bind', f'{empty_dir},{d}']
                for f in hidden_files:
                    firejail_args += ['--bind', f'{empty_file},{f}']

                original = os.environ.get('ORIGINAL_GEMINI', 'gemini')
                firejail_args += ['--', original] + args

                try:
                    subprocess.check_call(firejail_args)
                finally:
                    try:
                        os.rmdir(empty_dir)
                        os.unlink(empty_file)
                        os.rmdir(empty_file_dir)
                    except:
                        pass

            if __name__ == '__main__':
                main(sys.argv[1:])
            EOF

            chmod +x .nix-bin/gemini

            export PATH="$PWD/.nix-bin:$PATH"
            
            echo "🦀 FluxION ECS Development Environment"
            echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
            echo "Rust:   $(rustc --version)"
            echo "Cargo:  $(cargo --version)"
            echo "Docker: $(docker --version 2>/dev/null || echo 'not available')"
            echo ""
            echo "📦 Available commands:"
            echo "  nix build .#fluxion      - Build the Rust binary"
            echo "  nix build .#ha-addon         - Build HA addon Docker image"
            echo "  nix run .#test-all           - Run all tests"
            echo "  nix run .#test-ha-addon      - Test addon in Docker"
            echo "  nix run .#build-addon        - Build addon for distribution"
            echo "  nix flake check              - Run all checks"
            echo ""
            echo "🔨 Development:"
            echo "  cd fluxion"
            echo "  cargo test                   - Run tests"
            echo "  cargo clippy                 - Run linter"
            echo "  cargo build --release        - Build release binary"
            echo ""
          '';
        };

        # Comprehensive checks for CI/CD
        checks = {
          # Run Rust tests
          rust-tests = pkgs.stdenv.mkDerivation {
            name = "fluxion-tests";
            src = ./fluxion;

            nativeBuildInputs = [
              rustToolchain
              pkgs.pkg-config
            ];

            buildInputs = with pkgs; [
              openssl
            ];

            buildPhase = ''
              export CARGO_HOME=$TMPDIR/.cargo
              cargo test --workspace --verbose
            '';

            installPhase = ''
              touch $out
            '';
          };

          # Run clippy
          rust-clippy = pkgs.stdenv.mkDerivation {
            name = "fluxion-clippy";
            src = ./fluxion;

            nativeBuildInputs = [
              rustToolchain
              pkgs.pkg-config
            ];

            buildInputs = with pkgs; [
              openssl
            ];

            buildPhase = ''
              export CARGO_HOME=$TMPDIR/.cargo
              cargo clippy --workspace -- -D warnings
            '';

            installPhase = ''
              touch $out
            '';
          };

          # Check formatting
          rust-fmt = pkgs.stdenv.mkDerivation {
            name = "fluxion-fmt";
            src = ./fluxion;

            nativeBuildInputs = [
              rustToolchain
            ];

            buildPhase = ''
              cargo fmt --all -- --check
            '';

            installPhase = ''
              touch $out
            '';
          };

          # Check that addon structure is present
          addon-structure = pkgs.runCommand "addon-structure-check"
            {
              buildInputs = [ pkgs.coreutils ];
            } ''
            echo "Checking FluxION ECS addon structure..."
            src=${./.}
            
            # Check core addon files exist
            test -f "$src/fluxion/addon/config.yaml" || (echo "❌ Addon config.yaml missing" && exit 1)
            test -f "$src/fluxion/addon/Dockerfile" || (echo "❌ Addon Dockerfile missing" && exit 1)
            test -f "$src/fluxion/addon/build.yaml" || (echo "❌ Addon build.yaml missing" && exit 1)
            test -f "$src/fluxion/addon/README.md" || (echo "❌ Addon README.md missing" && exit 1)
            test -f "$src/fluxion/addon/rootfs/etc/services.d/fluxion/run" || (echo "❌ Addon run script missing" && exit 1)
            
            echo "✅ All addon files present"
            touch $out
          '';
        };
      }
    );
}
