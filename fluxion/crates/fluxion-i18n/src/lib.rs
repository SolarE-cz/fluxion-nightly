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

use fluent::{FluentArgs, FluentBundle, FluentResource};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use unic_langid::LanguageIdentifier;

/// Supported languages
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    /// English (default)
    #[default]
    English,
    /// Czech
    Czech,
}

impl Language {
    /// Get the language identifier string (e.g., "en", "cs")
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Czech => "cs",
        }
    }

    /// Get the language display name
    #[must_use]
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::English => "English",
            Self::Czech => "Čeština",
        }
    }

    /// List all supported languages
    pub const ALL: [Language; 2] = [Language::English, Language::Czech];

    /// Parse language from string code
    ///
    /// # Errors
    ///
    /// Returns `I18nError::UnsupportedLanguage` if the language code is not supported.
    pub fn from_code(code: &str) -> Result<Self, I18nError> {
        match code.to_lowercase().as_str() {
            "en" | "english" => Ok(Self::English),
            "cs" | "czech" | "cz" => Ok(Self::Czech),
            _ => Err(I18nError::UnsupportedLanguage(code.to_string())),
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::str::FromStr for Language {
    type Err = I18nError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_code(s)
    }
}

/// Translation errors
#[derive(Debug, Error)]
pub enum I18nError {
    /// Translation key not found
    #[error("Translation key not found: {0}")]
    KeyNotFound(String),

    /// Failed to load translation resource
    #[error("Failed to load translation resource: {0}")]
    LoadError(String),

    /// Unsupported language
    #[error("Unsupported language: {0}")]
    UnsupportedLanguage(String),

    /// Formatting error
    #[error("Failed to format translation: {0}")]
    FormatError(String),
}

/// Main i18n interface
pub struct I18n {
    bundles: Arc<Mutex<HashMap<String, FluentBundle<FluentResource>>>>,
    language: Language,
}

// Safety: I18n is safe to send between threads and share between threads
// because all access to the non-Send FluentBundle is protected by a Mutex.
// The bundles HashMap is never accessed without first acquiring the lock.
unsafe impl Send for I18n {}
unsafe impl Sync for I18n {}

impl std::fmt::Debug for I18n {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("I18n")
            .field("language", &self.language)
            .field("bundles", &"<FluentBundle>")
            .finish()
    }
}

impl I18n {
    /// Create a new i18n instance for the specified language
    ///
    /// # Errors
    ///
    /// Returns `I18nError::LoadError` if translation files cannot be loaded.
    pub fn new(language: Language) -> Result<Self, I18nError> {
        #[allow(clippy::arc_with_non_send_sync)]
        // Safe: We implement Send/Sync manually with proper justification
        let bundles = Arc::new(Mutex::new(HashMap::new()));
        let mut i18n = Self { bundles, language };

        // Load all translation domains
        i18n.load_domain("main")?;
        i18n.load_domain("web")?;
        i18n.load_domain("schedule")?;
        i18n.load_domain("config")?;

        Ok(i18n)
    }

    /// Load a translation domain (e.g., "main", "web", "schedule")
    fn load_domain(&mut self, domain: &str) -> Result<(), I18nError> {
        let lang_code = self.language.code();
        let ftl_content = Self::load_ftl_file(lang_code, domain)?;

        let resource = FluentResource::try_new(ftl_content)
            .map_err(|e| I18nError::LoadError(format!("Failed to parse {domain}.ftl: {e:?}")))?;

        let lang_id: LanguageIdentifier = lang_code
            .parse()
            .map_err(|e| I18nError::LoadError(format!("Invalid language ID: {e}")))?;

        let mut bundle = FluentBundle::new(vec![lang_id]);
        bundle
            .add_resource(resource)
            .map_err(|e| I18nError::LoadError(format!("Failed to add resource: {e:?}")))?;

        self.bundles.lock().insert(domain.to_string(), bundle);
        Ok(())
    }

    /// Load FTL file content
    fn load_ftl_file(lang_code: &str, domain: &str) -> Result<String, I18nError> {
        // For embedded resources in the binary
        match (lang_code, domain) {
            ("en", "main") => Ok(include_str!("../locales/en/main.ftl").to_string()),
            ("en", "web") => Ok(include_str!("../locales/en/web.ftl").to_string()),
            ("en", "schedule") => Ok(include_str!("../locales/en/schedule.ftl").to_string()),
            ("en", "config") => Ok(include_str!("../locales/en/config.ftl").to_string()),
            ("cs", "main") => Ok(include_str!("../locales/cs/main.ftl").to_string()),
            ("cs", "web") => Ok(include_str!("../locales/cs/web.ftl").to_string()),
            ("cs", "schedule") => Ok(include_str!("../locales/cs/schedule.ftl").to_string()),
            ("cs", "config") => Ok(include_str!("../locales/cs/config.ftl").to_string()),
            _ => Err(I18nError::LoadError(format!(
                "Translation file not found: {lang_code}/{domain}.ftl"
            ))),
        }
    }

    /// Get a translated string by key
    ///
    /// # Errors
    ///
    /// Returns `I18nError::KeyNotFound` if the translation key is not found in any domain.
    pub fn get(&self, key: &str) -> Result<String, I18nError> {
        self.format(key, None)
    }

    /// Format a translated string with arguments
    ///
    /// # Errors
    ///
    /// Returns `I18nError::KeyNotFound` if the translation key is not found.
    /// Returns `I18nError::FormatError` if formatting fails.
    pub fn format(&self, key: &str, args: Option<&FluentArgs>) -> Result<String, I18nError> {
        // Try each domain until we find the key
        let bundles = self.bundles.lock();
        for bundle in bundles.values() {
            if let Some(message) = bundle.get_message(key).and_then(|msg| msg.value()) {
                let mut errors = vec![];
                let value = bundle.format_pattern(message, args, &mut errors);

                if !errors.is_empty() {
                    return Err(I18nError::FormatError(format!(
                        "Formatting errors: {errors:?}"
                    )));
                }

                return Ok(value.to_string());
            }
        }

        Err(I18nError::KeyNotFound(key.to_string()))
    }

    /// Get the current language
    #[must_use]
    pub fn language(&self) -> Language {
        self.language
    }
}

/// Thread-safe wrapper for I18n suitable for use as a Bevy Resource
#[derive(Clone)]
pub struct I18nResource(pub Arc<I18n>);

impl I18nResource {
    /// Create a new `I18nResource`
    ///
    /// # Errors
    ///
    /// Returns `I18nError` if the i18n system fails to initialize.
    pub fn new(language: Language) -> Result<Self, I18nError> {
        Ok(Self(Arc::new(I18n::new(language)?)))
    }

    /// Get a reference to the underlying `I18n` instance
    #[must_use]
    pub fn inner(&self) -> &I18n {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_language_from_code() {
        assert_eq!(Language::from_code("en").unwrap(), Language::English);
        assert_eq!(Language::from_code("cs").unwrap(), Language::Czech);
        assert_eq!(Language::from_code("EN").unwrap(), Language::English);
        assert!(Language::from_code("fr").is_err());
    }

    #[test]
    fn test_language_code() {
        assert_eq!(Language::English.code(), "en");
        assert_eq!(Language::Czech.code(), "cs");
    }
}
