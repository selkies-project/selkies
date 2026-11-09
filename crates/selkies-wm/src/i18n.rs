/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Translations of the few texts the window manager shows itself: the window menu's entries,
//! the window list's heading, and a pipe menu loading. They are the desktop shell's own
//! catalogs, compiled in from its `locales/` directory so the two keep one table and one language
//! choice: the shell's `language` setting, else the locale environment, with English for
//! anything a catalog lacks.

use std::collections::HashMap;
use std::sync::RwLock;

/// Every translation file, by language code.
const LOCALES: [(&str, &str); 18] = [
    ("ar", include_str!("../../selkies-desktop/locales/ar.txt")),
    ("da", include_str!("../../selkies-desktop/locales/da.txt")),
    ("de", include_str!("../../selkies-desktop/locales/de.txt")),
    ("es", include_str!("../../selkies-desktop/locales/es.txt")),
    ("fil", include_str!("../../selkies-desktop/locales/fil.txt")),
    ("fr", include_str!("../../selkies-desktop/locales/fr.txt")),
    ("hi", include_str!("../../selkies-desktop/locales/hi.txt")),
    ("it", include_str!("../../selkies-desktop/locales/it.txt")),
    ("ja", include_str!("../../selkies-desktop/locales/ja.txt")),
    ("ko", include_str!("../../selkies-desktop/locales/ko.txt")),
    ("nl", include_str!("../../selkies-desktop/locales/nl.txt")),
    ("pt", include_str!("../../selkies-desktop/locales/pt.txt")),
    ("ru", include_str!("../../selkies-desktop/locales/ru.txt")),
    ("th", include_str!("../../selkies-desktop/locales/th.txt")),
    ("tr", include_str!("../../selkies-desktop/locales/tr.txt")),
    ("vi", include_str!("../../selkies-desktop/locales/vi.txt")),
    ("zh_cn", include_str!("../../selkies-desktop/locales/zh_cn.txt")),
    ("zh_tw", include_str!("../../selkies-desktop/locales/zh_tw.txt")),
];

/// The active language's table; `None` is English.
static ACTIVE: RwLock<Option<HashMap<&'static str, &'static str>>> = RwLock::new(None);

fn parse(text: &'static str) -> HashMap<&'static str, &'static str> {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once(" = "))
        .map(|(key, value)| (key.trim(), value.trim()))
        .filter(|(key, value)| !key.is_empty() && !value.is_empty())
        .collect()
}

/// The language code a locale tag selects (`de_DE.UTF-8`, `zh_TW`, `pt-BR`, `fil`), or `None`
/// for English or a language without a catalog.
pub fn normalize(tag: &str) -> Option<&'static str> {
    let lower = tag.trim().to_ascii_lowercase().replace('-', "_");
    let base = lower.split(['.', '@']).next().unwrap_or("");
    if base.is_empty() || base == "c" || base == "posix" {
        return None;
    }
    let (language, region) = match base.split_once('_') {
        Some((l, r)) => (l, Some(r)),
        None => (base, None),
    };
    let code = match (language, region) {
        ("zh", Some("tw")) | ("zh", Some("hk")) | ("zh", Some("mo")) | ("zh", Some("hant")) => "zh_tw",
        ("zh", _) => "zh_cn",
        ("tl", _) => "fil",
        (other, _) => other,
    };
    LOCALES.iter().map(|(name, _)| *name).find(|name| *name == code)
}

/// Choose the language: the desktop's `language` setting where set, else `LC_ALL`,
/// `LC_MESSAGES`, and `LANG` in that order.
pub fn select(language: Option<&str>) {
    let code = language.and_then(normalize).or_else(|| {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|var| std::env::var(var).ok())
            .find(|value| !value.trim().is_empty())
            .and_then(|value| normalize(&value))
    });
    let table = code.and_then(|code| LOCALES.iter().find(|(name, _)| *name == code));
    *ACTIVE.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = table.map(|(_, text)| parse(text));
}

/// The translation of an English text, or the text itself.
pub fn tr(key: &'static str) -> &'static str {
    ACTIVE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .and_then(|table| table.get(key).copied())
        .unwrap_or(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every catalog carries the window menu's entries.
    #[test]
    fn every_catalog_translates_the_window_menu() {
        for (name, text) in LOCALES {
            let table = parse(text);
            for key in [
                "Close",
                "Minimize",
                "Maximize",
                "Restore",
                "Move",
                "Resize",
                "Always on top",
                "Windows",
            ] {
                assert!(table.contains_key(key), "{name} lacks {key:?}");
            }
        }
        assert_eq!(normalize("de_DE.UTF-8"), Some("de"));
        assert_eq!(normalize("zh_HK"), Some("zh_tw"));
        assert_eq!(normalize("C"), None);
    }
}
