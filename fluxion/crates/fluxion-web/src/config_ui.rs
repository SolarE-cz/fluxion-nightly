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

use askama::Template;
use axum::{Router, response::Html, routing::get};
use fluxion_i18n::I18n;
use std::sync::Arc;

#[derive(Template, Clone)]
#[template(path = "config.html", escape = "none")]
pub struct ConfigTemplate {
    pub i18n: Arc<I18n>,
}

impl ConfigTemplate {
    pub fn t(&self, key: &str) -> String {
        self.i18n.get(key).unwrap_or_else(|_| key.to_owned())
    }
}

/// Router for config UI (currently unused, but kept for potential future use)
#[expect(dead_code, reason = "Config UI not yet enabled, kept for future use")]
pub fn router(i18n: Arc<I18n>) -> Router {
    let template = ConfigTemplate { i18n };
    Router::new().route(
        "/config",
        get(move || {
            let html = template
                .render()
                .unwrap_or_else(|e| format!("<h1>Template error</h1><p>{e}</p>"));
            async { Html(html) }
        }),
    )
}
