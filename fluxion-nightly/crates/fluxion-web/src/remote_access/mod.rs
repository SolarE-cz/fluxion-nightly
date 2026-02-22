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

mod api;
mod keygen;
mod mobile_api;
mod tor;

pub use api::{MobileBundleTemplate, RemoteAccessApiState, remote_access_routes};
pub use keygen::{DeviceEntry, DeviceStore};
pub use mobile_api::{MobileApiState, mobile_api_routes};
pub use tor::TorManager;
