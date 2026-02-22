// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.
//
// Licensed under the Creative Commons Attribution-NonCommercial-NoDerivatives 4.0 International
// (CC BY-NC-ND 4.0). You may use and share this file for non-commercial purposes only and you may not
// create derivatives. See <https://creativecommons.org/licenses/by-nc-nd/4.0/>.
//
// This software is provided "AS IS", without warranty of any kind.
//
// For commercial licensing, please contact: info@solare.cz

//! Version parsing and comparison module

use crate::error::{Result, UpgraderError};

/// Parse semver-like version strings (e.g., "0.2.38", "v0.2.38")
pub fn parse_version(s: &str) -> Result<(u32, u32, u32)> {
    let s = s.trim_start_matches('v').trim_start_matches('V');
    let parts: Vec<&str> = s.split('.').collect();

    if parts.len() != 3 {
        return Err(UpgraderError::VersionParse(format!(
            "Invalid version format: {s}, expected X.Y.Z"
        )));
    }

    let major = parts[0]
        .parse::<u32>()
        .map_err(|_| UpgraderError::VersionParse(format!("Invalid major version: {}", parts[0])))?;
    let minor = parts[1]
        .parse::<u32>()
        .map_err(|_| UpgraderError::VersionParse(format!("Invalid minor version: {}", parts[1])))?;
    let patch = parts[2]
        .parse::<u32>()
        .map_err(|_| UpgraderError::VersionParse(format!("Invalid patch version: {}", parts[2])))?;

    Ok((major, minor, patch))
}

/// Compare two version tuples, returns true if `remote` is newer than `local`
pub fn is_newer(local: &str, remote: &str) -> Result<bool> {
    let (local_major, local_minor, local_patch) = parse_version(local)?;
    let (remote_major, remote_minor, remote_patch) = parse_version(remote)?;

    if remote_major > local_major {
        return Ok(true);
    }
    if remote_major < local_major {
        return Ok(false);
    }

    if remote_minor > local_minor {
        return Ok(true);
    }
    if remote_minor < local_minor {
        return Ok(false);
    }

    Ok(remote_patch > local_patch)
}

/// Extract version from GitHub release tag (strips leading "v")
pub fn version_from_tag(tag: &str) -> &str {
    tag.trim_start_matches('v').trim_start_matches('V')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version() {
        assert_eq!(parse_version("0.2.38").unwrap(), (0, 2, 38));
        assert_eq!(parse_version("v0.2.38").unwrap(), (0, 2, 38));
        assert_eq!(parse_version("V0.2.38").unwrap(), (0, 2, 38));
        assert_eq!(parse_version("1.0.0").unwrap(), (1, 0, 0));
        assert_eq!(parse_version("10.20.30").unwrap(), (10, 20, 30));
    }

    #[test]
    fn test_parse_version_invalid() {
        assert!(parse_version("invalid").is_err());
        assert!(parse_version("1.2").is_err());
        assert!(parse_version("1.2.3.4").is_err());
        assert!(parse_version("a.b.c").is_err());
    }

    #[test]
    fn test_is_newer() {
        // Newer patch
        assert!(is_newer("0.2.38", "0.2.39").unwrap());
        // Same version
        assert!(!is_newer("0.2.38", "0.2.38").unwrap());
        // Older
        assert!(!is_newer("0.2.39", "0.2.38").unwrap());
        // Newer minor
        assert!(is_newer("0.2.38", "0.3.0").unwrap());
        // Newer major
        assert!(is_newer("0.2.38", "1.0.0").unwrap());
        // Major mismatch
        assert!(!is_newer("1.0.0", "0.9.99").unwrap());
    }

    #[test]
    fn test_version_from_tag_strips_v() {
        assert_eq!(version_from_tag("v0.2.38"), "0.2.38");
        assert_eq!(version_from_tag("V0.2.38"), "0.2.38");
        assert_eq!(version_from_tag("0.2.38"), "0.2.38");
    }
}
