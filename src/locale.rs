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
/// Sources, in order:
///
/// 1. **`sys-locale`**, which asks the OS's own preference API and hands back
///    BCP-47 tags: `GetUserPreferredUILanguages` on Windows,
///    `CFLocaleCopyPreferredLanguages` on macOS, and the `LANGUAGE`/`LC_*`/
///    `LANG` environment on Linux (normalised from `es_ES.UTF-8` to `es-ES`).
///    This comes first because of Windows: a GUI process launched from the
///    Start menu or a Run key inherits **no** `LANG`/`LC_*` at all — those are
///    POSIX conventions the Windows shell never sets — so an environment-only
///    resolver answered `None`, and "Match my system" was *always* English on
///    Windows no matter what the Language settings said.
///
///    **Linux precedence change.** `sys-locale` consults `LANGUAGE` (gettext's
///    colon-separated priority list, every entry in order) *before*
///    `LC_ALL`/`LC_MESSAGES`/`LANG`. The previous env-only resolver never read
///    `LANGUAGE`, so a host with `LANGUAGE=en_US:en LANG=es_ES.UTF-8` used to
///    resolve to Spanish and now resolves to English. That matches what
///    gettext-based desktop apps on the same host display, which is the point
///    of "Match my system".
/// 2. macOS `defaults read -g AppleLocale`, kept as a fallback for the case
///    where the CoreFoundation call yields nothing. It is the Region-format
///    locale rather than the UI language, which is why it is no longer first:
///    a Mac with Region = Spain but English as its preferred language used to
///    resolve to Spanish and now resolves to English.
/// 3. The POSIX environment, for a non-Windows host where `sys-locale` was
///    empty — e.g. a launcher that scrubbed the environment.
///
/// The `C`/`POSIX` locale is skipped at every layer: it means "no
/// preference", and honouring it would shadow a real `LANG` further down the
/// list.
fn system_language() -> Option<String> {
    if let Some(locale) = first_usable_locale(sys_locale::get_locales()) {
        return Some(locale);
    }
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
    first_usable_locale(
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .into_iter()
            .filter_map(|key| std::env::var(key).ok()),
    )
}

/// The first candidate that actually names a language.
///
/// Blank entries and the `C`/`POSIX` locale are skipped rather than returned,
/// so `LC_ALL=C LANG=es_ES.UTF-8` still resolves to Spanish — `sys-locale`
/// passes `C` through verbatim and would otherwise win with it. Pure so it can
/// be tested without touching the environment.
fn first_usable_locale(candidates: impl IntoIterator<Item = String>) -> Option<String> {
    candidates
        .into_iter()
        .map(|c| c.trim().to_string())
        .find(|c| !c.is_empty() && c != "C" && c != "POSIX")
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

    /// `sys-locale` answers in BCP-47 (`GetUserPreferredUILanguages` on
    /// Windows, `CFLocaleCopyPreferredLanguages` on macOS), so the resolver
    /// must accept hyphenated region tags — not just the POSIX `es_ES.UTF-8`
    /// shape it was written against.
    #[test]
    fn bcp47_tags_from_sys_locale_resolve_to_the_shipped_language() {
        for raw in ["es-ES", "es-MX", "es-419", "es-Latn-ES"] {
            assert_eq!(match_system(Some(raw)), "es", "input {raw:?}");
        }
        for raw in ["en-US", "en-GB", "en-001"] {
            assert_eq!(match_system(Some(raw)), "en", "input {raw:?}");
        }
        assert_eq!(match_system(Some("fr-CA")), FALLBACK);
    }

    /// `C`/`POSIX` mean "no preference" and must never shadow a real locale
    /// further down the list — `sys-locale` passes them through verbatim.
    #[test]
    fn the_posix_c_locale_is_skipped_in_favour_of_a_real_one() {
        let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            first_usable_locale(owned(&["C", "es-ES"])),
            Some("es-ES".to_string())
        );
        assert_eq!(
            first_usable_locale(owned(&["", "  ", "POSIX", " en-GB "])),
            Some("en-GB".to_string())
        );
        assert_eq!(first_usable_locale(owned(&["C", "POSIX", ""])), None);
        assert_eq!(first_usable_locale(Vec::<String>::new()), None);
    }

    /// On Linux `sys-locale` expands `LANGUAGE=en_US:en` ahead of `LANG`, so
    /// the list it hands over for `LANGUAGE=en_US:en LANG=es_ES.UTF-8` is
    /// `["en-US", "en", "es-ES"]`. The first entry wins — `LANGUAGE` outranks
    /// `LANG`, which the old env-only resolver (that never read `LANGUAGE`)
    /// did not do. Pinned so the precedence cannot drift silently.
    #[test]
    fn language_list_entries_outrank_lang_in_sys_locale_order() {
        let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let first = first_usable_locale(owned(&["en-US", "en", "es-ES"]));
        assert_eq!(first, Some("en-US".to_string()));
        assert_eq!(match_system(first.as_deref()), "en");
        // A `C` at the head of LANGUAGE still yields to the real entries.
        assert_eq!(
            first_usable_locale(owned(&["C", "en-US", "es-ES"])),
            Some("en-US".to_string())
        );
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
