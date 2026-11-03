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

//! Log capture for tests. One global subscriber writes each event to its own thread's buffer:
//! `#[tokio::test]` runs on one thread, so the test sees the lines its own code emitted.

use std::cell::RefCell;

thread_local! {
    static CAPTURED_LOGS: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

/// Writes an event into this thread's capture buffer, if one is open
struct ThreadLogWriter;

impl std::io::Write for ThreadLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURED_LOGS.with(|logs| {
            if let Some(logs) = logs.borrow_mut().as_mut() {
                logs.extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Start capturing log lines (INFO and above) emitted on this thread
pub(crate) fn start_log_capture() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(|| ThreadLogWriter)
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
    CAPTURED_LOGS.with(|logs| *logs.borrow_mut() = Some(Vec::new()));
}

/// The lines captured since `start_log_capture`
pub(crate) fn captured_logs() -> String {
    let logs = CAPTURED_LOGS.with(|logs| logs.borrow_mut().take().unwrap_or_default());
    String::from_utf8_lossy(&logs).into_owned()
}
