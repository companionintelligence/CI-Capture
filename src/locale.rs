//! UI language selection.
//!
//! Every user-facing string lives in `locales/*.yml` and is fetched through
//! `rust_i18n::t!`. This module owns the one decision that macro cannot make
//! for itself: *which* locale a freshly launched window should speak.
//!
//! **Why a per-process global is the right scope here.** Each window is its own
//! OS process (see the process model in `main.rs`), so `rust_i18n`'s global
//! locale is naturally per-window. There is no shared mutable locale to race
//! on, and a window launched from the tray inherits the saved config exactly
//! like it inherits every other setting.
//!
//! **Why the OS locale is only a fallback.** `AppConfig::ui_language` defaults
//! to [`Language::System`], which resolves against the host at startup. Once a
//! user picks a language explicitly we store that choice and stop consulting
//! the OS — someone who runs an English macOS but wants a Spanish app should
//! not have their choice silently reverted by a system update.
//!
//! ## Adding a language
//!
//! 1. Add a variant to [`Language`] and wire it through `code`, `ALL`, and
//!    `label` (which is deliberately written in the language itself).
//! 2. Add the matching `es:`-style key to **every** file under `locales/`.
//!    `cargo test` fails on any key missing from any locale — see
//!    `locale_catalogue_is_complete`.
//! 3. Check the glyphs actually render. The bundled Montserrat
//!    (`theme::install_fonts`) covers Latin and Latin-Extended only. A CJK or
//!    Cyrillic language needs a font bundled alongside it, and egui has no
//!    bidirectional layout, so right-to-left languages are not viable yet.

use serde::{Deserialize, Serialize};

/// A language the UI can be displayed in.
///
/// Serialized by its BCP-47 code so the on-disk config stays readable and a
/// future language can be added without a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Language {
    /// Follow the host OS, falling back to English when it names a language we
    /// do not ship.
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "en")]
    English,
    #[serde(rename = "es")]
    Spanish,
}

impl Language {
    /// Every language offered in the Settings picker, in display order.
    pub const ALL: [Language; 3] = [Language::System, Language::English, Language::Spanish];

    /// The locale code handed to `rust_i18n`. [`Language::System`] has none of
    /// its own — it resolves through [`resolve`].
    pub fn code(self) -> Option<&'static str> {
        match self {
            Language::System => None,
            Language::English => Some("en"),
            Language::Spanish => Some("es"),
        }
    }

    /// The name shown in the picker.
    ///
    /// Written in the language it names — a user hunting for their own language
    /// is looking for "Español", not for the English word "Spanish", and may
    /// not be able to read the rest of the list to find it.
    pub fn label(self) -> String {
        match self {
            Language::System => rust_i18n::t!("settings.language.system").to_string(),
            Language::English => "English".to_string(),
            Language::Spanish => "Español".to_string(),
        }
    }
}

/// The languages this build ships, as locale codes. Used by the completeness test.
pub const SHIPPED: [&str; 2] = ["en", "es"];

/// Fallback for any locale we do not ship, and for an unreadable OS setting.
pub const FALLBACK: &str = "en";

/// Resolve a [`Language`] to a concrete locale code.
///
/// [`Language::System`] consults the host; anything the host names that we do
/// not ship degrades to [`FALLBACK`] rather than rendering raw message keys.
pub fn resolve(language: Language) -> &'static str {
    match language.code() {
        Some(code) => code,
        None => match_system(system_language().as_deref()),
    }
}

/// Match a host locale string against the languages we ship.
///
/// Split out from [`resolve`] so it can be tested without a host: the OS hands
/// back anything from `es` to `es-419` to `es_ES.UTF-8`, and only the primary
/// subtag is meaningful to us.
fn match_system(raw: Option<&str>) -> &'static str {
    let Some(raw) = raw else {
        return FALLBACK;
    };
    let primary = raw
        .split(['-', '_', '.'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    SHIPPED
        .iter()
        .find(|code| **code == primary)
        .copied()
        .unwrap_or(FALLBACK)
}

/// Read the host's preferred language.
///
/// macOS answers `AppleLanguages` through `defaults`, which is the only source
/// that reflects the Language & Region pane; `LANG` is frequently unset for
/// GUI apps there. Elsewhere the POSIX environment is authoritative.
fn system_language() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        if let Ok(out) = Command::new("defaults")
            .args(["read", "-g", "AppleLocale"])
            .output()
            && out.status.success()
            && let Ok(s) = String::from_utf8(out.stdout)
            && !s.trim().is_empty()
        {
            return Some(s.trim().to_string());
        }
    }
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(key)
            && !v.trim().is_empty()
            && v != "C"
            && v != "POSIX"
        {
            return Some(v);
        }
    }
    None
}

/// Apply a language to this process. Call once per window, before first paint.
pub fn apply(language: Language) {
    rust_i18n::set_locale(resolve(language));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_locales_match_on_the_primary_subtag_only() {
        // The shapes an OS actually hands back.
        for raw in ["es", "es-ES", "es_ES.UTF-8", "es-419", "ES"] {
            assert_eq!(match_system(Some(raw)), "es", "input {raw:?}");
        }
        for raw in ["en", "en-GB", "en_US.UTF-8"] {
            assert_eq!(match_system(Some(raw)), "en", "input {raw:?}");
        }
    }

    #[test]
    fn unshipped_and_unreadable_locales_fall_back_to_english() {
        // A language we don't ship must render English, never raw message keys.
        for raw in ["fr", "ja-JP", "zxx", "", "   "] {
            assert_eq!(match_system(Some(raw)), FALLBACK, "input {raw:?}");
        }
        assert_eq!(match_system(None), FALLBACK);
    }

    #[test]
    fn an_explicit_choice_never_consults_the_host() {
        // The whole point of storing a choice: a system-locale change must not
        // silently revert it.
        assert_eq!(resolve(Language::Spanish), "es");
        assert_eq!(resolve(Language::English), "en");
    }

    #[test]
    fn every_shipped_language_is_offered_and_labelled() {
        for code in SHIPPED {
            assert!(
                Language::ALL.iter().any(|l| l.code() == Some(code)),
                "{code} is shipped but missing from the Settings picker"
            );
        }
        // System has no code of its own but must still be offerable.
        assert!(Language::ALL.contains(&Language::System));
        for lang in Language::ALL {
            if lang != Language::System {
                assert!(!lang.label().is_empty());
            }
        }
    }

    #[test]
    fn language_round_trips_through_config_serialization() {
        for lang in Language::ALL {
            let json = serde_json::to_string(&lang).unwrap();
            let back: Language = serde_json::from_str(&json).unwrap();
            assert_eq!(lang, back, "round-trip failed for {lang:?}");
        }
        // Codes are the stable on-disk form.
        assert_eq!(serde_json::to_string(&Language::Spanish).unwrap(), "\"es\"");
        assert_eq!(
            serde_json::to_string(&Language::System).unwrap(),
            "\"system\""
        );
    }

    #[test]
    fn an_unknown_language_in_config_is_rejected_not_silently_defaulted() {
        // Better to surface a corrupt config than to render the wrong language.
        assert!(serde_json::from_str::<Language>("\"kl\"").is_err());
    }
}
