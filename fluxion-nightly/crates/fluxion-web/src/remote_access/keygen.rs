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

use chrono::{DateTime, Utc};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use x25519_dalek::{PublicKey, StaticSecret};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub id: String,
    pub name: String,
    pub access_mode: String,
    pub pubkey_base32: String,
    pub created_at: DateTime<Utc>,
    pub last_seen: Option<DateTime<Utc>>,
}

#[derive(Debug)]
pub struct DeviceStore {
    devices_path: PathBuf,
    auth_dir: PathBuf,
}

/// Generate a new x25519 keypair for Tor client authorization.
pub fn generate_client_keypair() -> (StaticSecret, PublicKey) {
    let secret = StaticSecret::random_from_rng(OsRng);
    let public = PublicKey::from(&secret);
    (secret, public)
}

/// Encode a public key as base32 (uppercase, no padding) for Tor auth files.
fn encode_pubkey_base32(pubkey: &PublicKey) -> String {
    base32::encode(
        base32::Alphabet::Rfc4648 { padding: false },
        pubkey.as_bytes(),
    )
}

/// Encode a private key as base64 for the QR code payload.
pub fn encode_privkey_base64(secret: &StaticSecret) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(secret.to_bytes())
}

impl DeviceStore {
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        let auth_dir = data_dir.join("tor").join("authorized_clients");
        let devices_path = data_dir.join("tor").join("devices.json");
        Self {
            devices_path,
            auth_dir,
        }
    }

    /// Load all devices from persistent storage.
    #[must_use]
    pub fn load_devices(&self) -> Vec<DeviceEntry> {
        match std::fs::read_to_string(&self.devices_path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// Save devices to persistent storage.
    fn save_devices(&self, devices: &[DeviceEntry]) -> std::io::Result<()> {
        if let Some(parent) = self.devices_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(devices).map_err(std::io::Error::other)?;
        std::fs::write(&self.devices_path, json)
    }

    /// Register a new device: generates keypair, writes .auth file, persists metadata.
    /// Returns `(device_entry, private_key_base64)`.
    pub fn register_device(
        &self,
        name: &str,
        access_mode: &str,
    ) -> std::io::Result<(DeviceEntry, String)> {
        let (secret, public) = generate_client_keypair();
        let device_id = uuid::Uuid::new_v4().to_string();
        let pubkey_b32 = encode_pubkey_base32(&public);
        let privkey_b64 = encode_privkey_base64(&secret);

        // Write Tor authorized_clients file
        self.write_auth_file(&device_id, &pubkey_b32)?;

        let entry = DeviceEntry {
            id: device_id,
            name: name.to_owned(),
            access_mode: access_mode.to_owned(),
            pubkey_base32: pubkey_b32,
            created_at: Utc::now(),
            last_seen: None,
        };

        let mut devices = self.load_devices();
        devices.push(entry.clone());
        self.save_devices(&devices)?;

        Ok((entry, privkey_b64))
    }

    /// Revoke a device: remove .auth file and device metadata.
    pub fn revoke_device(&self, device_id: &str) -> std::io::Result<bool> {
        let mut devices = self.load_devices();
        let original_len = devices.len();
        devices.retain(|d| d.id != device_id);

        if devices.len() == original_len {
            return Ok(false);
        }

        self.delete_auth_file(device_id)?;
        self.save_devices(&devices)?;
        Ok(true)
    }

    /// Write a `.auth` file for Tor client authorization.
    fn write_auth_file(&self, device_id: &str, pubkey_base32: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.auth_dir)?;
        let auth_path = self.auth_dir.join(format!("{device_id}.auth"));
        let content = format!("descriptor:x25519:{pubkey_base32}");
        std::fs::write(auth_path, content)
    }

    /// Delete the `.auth` file for a device.
    fn delete_auth_file(&self, device_id: &str) -> std::io::Result<()> {
        let auth_path = self.auth_dir.join(format!("{device_id}.auth"));
        match std::fs::remove_file(auth_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_keypair() {
        let (secret, public) = generate_client_keypair();
        let derived_public = PublicKey::from(&secret);
        assert_eq!(public.as_bytes(), derived_public.as_bytes());
    }

    #[test]
    fn test_pubkey_base32_encoding() {
        let (_secret, public) = generate_client_keypair();
        let encoded = encode_pubkey_base32(&public);
        // x25519 public key is 32 bytes → base32 with no padding is 52 chars
        assert_eq!(encoded.len(), 52);
        assert!(encoded.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn test_device_store_crud() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DeviceStore::new(tmp.path());

        // Register
        let (entry, privkey) = store.register_device("My Phone", "full").unwrap();
        assert_eq!(entry.name, "My Phone");
        assert_eq!(entry.access_mode, "full");
        assert!(!privkey.is_empty());

        // Load
        let devices = store.load_devices();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, entry.id);

        // Auth file exists
        let auth_path = tmp
            .path()
            .join("tor")
            .join("authorized_clients")
            .join(format!("{}.auth", entry.id));
        assert!(auth_path.exists());
        let auth_content = std::fs::read_to_string(&auth_path).unwrap();
        assert!(auth_content.starts_with("descriptor:x25519:"));

        // Revoke
        let revoked = store.revoke_device(&entry.id).unwrap();
        assert!(revoked);
        assert!(store.load_devices().is_empty());
        assert!(!auth_path.exists());

        // Revoke nonexistent
        let revoked = store.revoke_device("nonexistent").unwrap();
        assert!(!revoked);
    }
}
