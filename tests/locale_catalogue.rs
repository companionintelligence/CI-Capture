//! The catalogue is only as good as its weakest locale.
//!
//! `rust_i18n!(fallback = "en")` means a missing Spanish key silently renders
//! English. That is the right runtime behaviour — a half-translated build stays
//! usable — but it also means an untranslated string ships without anyone
//! noticing. This test is what notices.
//!
//! It reads `locales/*.yml` directly rather than going through `t!`, because
//! the point is to inspect the catalogue itself, not what the fallback chain
//! papers over at runtime.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Locales every key must define. Mirrors `crate::locale::SHIPPED`, which the
/// binary crate does not re-export to integration tests.
const SHIPPED: [&str; 2] = ["en", "es"];

fn locales_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("locales")
}

/// Parse the `key: { locale: value }` shape without pulling in a YAML crate.
///
/// The catalogue format is deliberately flat and machine-written, so a
/// hand-rolled reader is enough — and it keeps `serde_yaml` (unmaintained) out
/// of the dependency tree for a test.
fn parse(text: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("");
        if line.trim().is_empty() {
            continue;
        }
        let indented = line.starts_with([' ', '\t']);
        let Some((lhs, rhs)) = line.split_once(':') else {
            continue;
        };
        let (lhs, rhs) = (lhs.trim(), rhs.trim());
        if !indented {
            if lhs == "_version" {
                current = None;
                continue;
            }
            current = Some(lhs.to_string());
            out.entry(lhs.to_string()).or_default();
        } else if let Some(key) = &current
            && !rhs.is_empty()
        {
            out.entry(key.clone())
                .or_default()
                .insert(lhs.to_string(), rhs.trim_matches('"').to_string());
        }
    }
    out
}

fn catalogue() -> BTreeMap<String, BTreeMap<String, String>> {
    let dir = locales_dir();
    let mut all = BTreeMap::new();
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for (key, langs) in parse(&text) {
            if let Some(prev) = all.insert(key.clone(), langs) {
                panic!(
                    "message key {key:?} is defined twice across locales/*.yml \
                     (second definition in {}); rust-i18n merges files, so one \
                     silently wins — namespace the key instead. First had: {prev:?}",
                    path.display()
                );
            }
        }
    }
    all
}

#[test]
fn every_key_is_translated_into_every_shipped_language() {
    let cat = catalogue();
    assert!(
        !cat.is_empty(),
        "locales/ produced no keys — is the path right?"
    );

    let mut missing: Vec<String> = Vec::new();
    for (key, langs) in &cat {
        for locale in SHIPPED {
            match langs.get(locale) {
                None => missing.push(format!("  {key}  → no {locale}:")),
                Some(v) if v.trim().is_empty() => {
                    missing.push(format!("  {key}  → {locale}: is empty"))
                }
                Some(_) => {}
            }
        }
    }
    assert!(
        missing.is_empty(),
        "{} untranslated message(s). The `fallback = \"en\"` in main.rs means these \
         render English at runtime instead of failing, so nothing else will catch them:\n{}",
        missing.len(),
        missing.join("\n")
    );
}

#[test]
fn no_locale_value_is_left_as_a_copy_of_the_key() {
    // A common half-finished state: the key pasted in as its own value.
    let cat = catalogue();
    for (key, langs) in &cat {
        for (locale, value) in langs {
            assert_ne!(
                value, key,
                "{key} in {locale} is still the message id, not a translation"
            );
        }
    }
}

#[test]
fn spanish_is_actually_distinct_from_english() {
    // Catches a bulk-copied `es:` block. Identical strings are legitimate for
    // product names and a few loanwords, so this asserts on the ratio, not on
    // any single key.
    let cat = catalogue();
    let comparable: Vec<_> = cat
        .iter()
        .filter_map(|(k, l)| Some((k, l.get("en")?, l.get("es")?)))
        .collect();
    assert!(
        comparable.len() >= 3,
        "too few keys to judge; catalogue has {} comparable entries",
        comparable.len()
    );
    let identical = comparable.iter().filter(|(_, en, es)| en == es).count();
    let ratio = identical as f32 / comparable.len() as f32;
    assert!(
        ratio < 0.5,
        "{identical}/{} Spanish strings are byte-identical to English — \
         the `es:` block looks copied rather than translated",
        comparable.len()
    );
}
