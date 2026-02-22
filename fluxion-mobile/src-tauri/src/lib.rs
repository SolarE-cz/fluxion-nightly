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

mod cache;
mod commands;
mod credentials;
mod state;
mod tor;

use tracing::info;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    info!("Starting FluxION Mobile");

    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_os::init());

    // Barcode scanner plugin is only available on mobile targets
    #[cfg(mobile)]
    {
        builder = builder.plugin(tauri_plugin_barcode_scanner::init());
    }

    builder
        .invoke_handler(tauri::generate_handler![
            commands::scan_qr,
            commands::get_state,
            commands::save_controls,
            commands::get_cached_ui,
            commands::check_ui_update,
            commands::get_connection_info,
            commands::is_pin_set,
            commands::set_pin,
            commands::verify_pin,
            commands::remove_pin,
        ])
        .setup(|app| {
            use tauri::Manager;
            let data_dir = app
                .path()
                .app_data_dir()
                .expect("failed to get app data dir");
            let app_handle = app.handle().clone();
            let mut app_state = state::AppState::new(app_handle.clone(), data_dir);

            // Restore persisted credentials
            if let Some(conn) = credentials::load_connection(&app_handle) {
                info!("Restored persisted connection: {}", conn.instance_name);
                // Re-configure Tor client with restored credentials
                if let Ok(ref bytes) = commands::base64_decode(&conn.client_auth_key_b64) {
                    if bytes.len() == 32 {
                        let mut key = [0u8; 32];
                        key.copy_from_slice(bytes);
                        app_state
                            .tor
                            .get_mut()
                            .configure(conn.onion_address.clone(), key);
                    }
                }
                *app_state.connection.get_mut() = Some(conn);
            }
            *app_state.settings.get_mut() = credentials::load_settings(&app_handle);

            app.manage(app_state);
            info!("FluxION Mobile setup complete");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running FluxION Mobile");
}
