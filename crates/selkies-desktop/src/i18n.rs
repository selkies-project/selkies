//! Translations of the text the desktop shows.  The English text is the
//! key; each language is a `key = text` file under `locales/` compiled into
//! the binary, chosen from the `language` setting or the locale environment
//! variables, with English for anything a file lacks.

use std::collections::HashMap;
use std::sync::RwLock;

/// Every translation file, by language code.
const LOCALES: [(&str, &str); 18] = [
    ("ar", include_str!("../locales/ar.txt")),
    ("da", include_str!("../locales/da.txt")),
    ("de", include_str!("../locales/de.txt")),
    ("es", include_str!("../locales/es.txt")),
    ("fil", include_str!("../locales/fil.txt")),
    ("fr", include_str!("../locales/fr.txt")),
    ("hi", include_str!("../locales/hi.txt")),
    ("it", include_str!("../locales/it.txt")),
    ("ja", include_str!("../locales/ja.txt")),
    ("ko", include_str!("../locales/ko.txt")),
    ("nl", include_str!("../locales/nl.txt")),
    ("pt", include_str!("../locales/pt.txt")),
    ("ru", include_str!("../locales/ru.txt")),
    ("th", include_str!("../locales/th.txt")),
    ("tr", include_str!("../locales/tr.txt")),
    ("vi", include_str!("../locales/vi.txt")),
    ("zh_cn", include_str!("../locales/zh_cn.txt")),
    ("zh_tw", include_str!("../locales/zh_tw.txt")),
];

/// The active language and its table; `None` is English.
static ACTIVE: RwLock<Option<(&'static str, HashMap<&'static str, &'static str>)>> = RwLock::new(None);

/// Parses a translation file into its table.
fn parse(text: &'static str) -> HashMap<&'static str, &'static str> {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once(" = "))
        .map(|(key, value)| (key.trim(), value.trim()))
        .filter(|(key, value)| !key.is_empty() && !value.is_empty())
        .collect()
}

/// The language code a locale tag selects, or `None` for English or an
/// unknown language.
///
/// # Arguments
/// * `tag` - A locale such as `de_DE.UTF-8`, `zh_TW`, `pt-BR` or `fil`.
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

/// The language the environment asks for, from `LC_ALL`, `LC_MESSAGES`
/// and `LANG` in that order.
pub fn detect() -> Option<&'static str> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .find(|value| !value.trim().is_empty())
        .and_then(|value| normalize(&value))
}

/// Chooses the language, from the setting when given, else the environment.
///
/// # Arguments
/// * `language` - The `language` setting, if any.
pub fn select(language: Option<&str>) {
    let code = language.and_then(normalize).or_else(detect);
    let table = code.and_then(|code| LOCALES.iter().find(|(name, _)| *name == code));
    let mut active = ACTIVE.write().unwrap_or_else(|poisoned| poisoned.into_inner());
    *active = table.map(|(name, text)| (*name, parse(text)));
}

/// The active language code, `en` for English.
pub fn language() -> &'static str {
    ACTIVE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .map_or("en", |(name, _)| name)
}

/// The translation of a text, or the text itself.
///
/// # Arguments
/// * `key` - The English text.
pub fn tr(key: &'static str) -> &'static str {
    ACTIVE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .and_then(|(_, table)| table.get(key).copied())
        .unwrap_or(key)
}

/// A translation with its placeholders filled in.
///
/// # Arguments
/// * `key` - The English text, with `{name}` placeholders.
/// * `values` - The placeholder names and their values.
pub fn trf(key: &'static str, values: &[(&str, &str)]) -> String {
    let mut text = tr(key).to_string();
    for (name, value) in values {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

/// A text in every language that has it, for localized desktop file keys.
///
/// # Arguments
/// * `key` - The English text.
///
/// # Returns
/// Language codes as desktop files spell them, each with its translation.
pub fn all(key: &str) -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    for (name, text) in LOCALES {
        if let Some(value) = parse(text).get(key) {
            let code = match name {
                "zh_cn" => "zh_CN",
                "zh_tw" => "zh_TW",
                other => other,
            };
            out.push((code, *value));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_locale_tags() {
        assert_eq!(normalize("de_DE.UTF-8"), Some("de"));
        assert_eq!(normalize("zh_TW"), Some("zh_tw"));
        assert_eq!(normalize("zh_HK.utf8"), Some("zh_tw"));
        assert_eq!(normalize("zh_CN"), Some("zh_cn"));
        assert_eq!(normalize("zh"), Some("zh_cn"));
        assert_eq!(normalize("pt-BR"), Some("pt"));
        assert_eq!(normalize("tl_PH"), Some("fil"));
        assert_eq!(normalize("en_US.UTF-8"), None);
        assert_eq!(normalize("C"), None);
        assert_eq!(normalize("xx"), None);
    }

    #[test]
    fn every_locale_covers_the_same_keys() {
        let keys: Vec<&str> = parse(LOCALES[0].1).keys().copied().collect();
        for (name, text) in LOCALES {
            let table = parse(text);
            for key in &keys {
                assert!(table.contains_key(key), "{name} lacks {key}");
            }
            assert_eq!(table.len(), keys.len(), "{name} has extra keys");
        }
        assert!(keys.contains(&"Settings"));
    }

    /// Every `tr("…")` and `trf("…")` key in the sources, except this
    /// module's own tests.
    fn source_keys() -> Vec<String> {
        fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") && !path.ends_with("i18n.rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    for call in ["tr(\"", "trf(\""] {
                        for (at, _) in text.match_indices(call) {
                            let before = text[..at].chars().next_back();
                            if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                                continue;
                            }
                            let rest = &text[at + call.len()..];
                            if let Some(end) = rest.find('"') {
                                out.push(rest[..end].to_string());
                            }
                        }
                    }
                }
            }
        }
        let mut keys = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut keys,
        );
        keys.sort();
        keys.dedup();
        keys
    }

    #[test]
    fn every_source_key_is_translated() {
        let keys = source_keys();
        assert!(keys.len() > 100, "found only {} keys", keys.len());
        let table = parse(LOCALES[0].1);
        let missing: Vec<&String> = keys
            .iter()
            .filter(|key| !table.contains_key(key.as_str()))
            .collect();
        assert!(missing.is_empty(), "keys without a translation: {missing:?}");
    }

    #[test]
    fn translates_and_fills() {
        select(Some("es"));
        assert_eq!(language(), "es");
        assert_eq!(tr("Settings"), "Ajustes");
        assert_eq!(tr("Not a key"), "Not a key");
        assert_eq!(trf("{n} items", &[("n", "3")]), "3 elementos");
        select(Some("en"));
        assert_eq!(language(), "en");
        assert_eq!(tr("Settings"), "Settings");
        assert!(
            all("Files")
                .iter()
                .any(|(code, text)| *code == "zh_CN" && *text == "文件")
        );
    }
}
