// SPDX-FileCopyrightText: © 2025 Daniel Havlicek <daniel@solare.cz>
// SPDX-License-Identifier: CC-BY-NC-ND-4.0

//! Tiny binary that outputs the FluxION workspace version.
//!
//! This is the single source of truth for versioning across the project.
//! The version is inherited from the workspace Cargo.toml at compile time.
//!
//! Usage:
//!   fluxion-version        # prints version (e.g., "0.1.21")
//!   fluxion-version --help # prints help
//!
//! In Nix:
//!   nix run .#version      # prints version

/// Version from workspace Cargo.toml, injected at compile time
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 {
        match args[1].as_str() {
            "--help" | "-h" => {
                eprintln!("fluxion-version - Output FluxION workspace version");
                eprintln!();
                eprintln!("Usage: fluxion-version [OPTIONS]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  -h, --help    Print this help message");
                eprintln!("  -v, --version Print version (same as no args)");
                eprintln!();
                eprintln!("The version is derived from workspace Cargo.toml at compile time.");
            }
            "--version" | "-v" => {
                print!("{VERSION}");
            }
            other => {
                eprintln!("Unknown option: {other}");
                eprintln!("Use --help for usage information.");
                std::process::exit(1);
            }
        }
    } else {
        // Default: just print the version without newline for easy scripting
        print!("{VERSION}");
    }
}
