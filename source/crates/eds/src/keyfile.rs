//! A small reader for the GLib key-file format (`GKeyFile`) that EDS uses for
//! every source's configuration - the `.source` files under
//! `~/.config/evolution/sources/`, and the identical text the registry hands
//! out in each source object's `Data` property. Pure, with no `glib`
//! dependency: this crate stays `zbus`-only, like `lookout-goa`.
//!
//! Only reading is supported, and only what EDS writes: `[Group]` headers,
//! `key=value` entries (including translated `key[locale]=value` forms), `#`
//! comments and blank lines. As in GLib, any other line makes the whole file
//! invalid.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyFileError {
    #[error("line {0}: key/value before the first group")]
    EntryOutsideGroup(usize),
    #[error("line {0}: not a group, entry or comment")]
    InvalidLine(usize),
}

#[derive(Debug, Clone, Default)]
pub struct KeyFile {
    /// Groups in file order. A group repeated later in the file is merged into
    /// its first occurrence, as GLib does.
    groups: Vec<(String, Vec<(String, String)>)>,
}

impl KeyFile {
    pub fn parse(text: &str) -> Result<KeyFile, KeyFileError> {
        let mut file = KeyFile::default();
        let mut current: Option<usize> = None;
        for (index, raw) in text.lines().enumerate() {
            let line_no = index + 1;
            let line = raw.trim_start();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[') {
                let name = name.trim_end().strip_suffix(']').ok_or(KeyFileError::InvalidLine(line_no))?;
                if name.is_empty() || name.contains(['[', ']']) {
                    return Err(KeyFileError::InvalidLine(line_no));
                }
                current = Some(match file.groups.iter().position(|(g, _)| g == name) {
                    Some(existing) => existing,
                    None => {
                        file.groups.push((name.to_string(), Vec::new()));
                        file.groups.len() - 1
                    }
                });
                continue;
            }
            let (key, value) = line.split_once('=').ok_or(KeyFileError::InvalidLine(line_no))?;
            let key = key.trim_end();
            if key.is_empty() {
                return Err(KeyFileError::InvalidLine(line_no));
            }
            let group = current.ok_or(KeyFileError::EntryOutsideGroup(line_no))?;
            file.groups[group].1.push((key.to_string(), unescape(value.trim_start())));
        }
        Ok(file)
    }

    pub fn has_group(&self, group: &str) -> bool {
        self.groups.iter().any(|(g, _)| g == group)
    }

    /// The (unescaped) value of `key` in `group`. When a key appears more than
    /// once, the last occurrence wins, as in GLib.
    pub fn get(&self, group: &str, key: &str) -> Option<&str> {
        let (_, entries) = self.groups.iter().find(|(g, _)| g == group)?;
        entries.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// `get`, with empty values treated as absent - EDS writes `Parent=` and
    /// `BackendName=` with no value rather than leaving the key out.
    pub fn get_non_empty(&self, group: &str, key: &str) -> Option<&str> {
        self.get(group, key).filter(|v| !v.is_empty())
    }

    /// GLib's boolean spellings: `true`/`false` and `1`/`0`. Anything else
    /// reads as absent.
    pub fn get_bool(&self, group: &str, key: &str) -> Option<bool> {
        match self.get(group, key)?.trim_end() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        }
    }

    /// The translated value of `key` for the first of `locales` that has one
    /// (`key[de_DE]`, `key[de]`, ...), falling back to the untranslated `key`.
    /// `locales` is in preference order, as [`language_names`] returns it.
    pub fn get_locale_string(&self, group: &str, key: &str, locales: &[String]) -> Option<&str> {
        locales
            .iter()
            .find_map(|locale| self.get_non_empty(group, &format!("{key}[{locale}]")))
            .or_else(|| self.get(group, key))
    }
}

/// GKeyFile value escapes: `\s`, `\n`, `\t`, `\r` and `\\`. An unknown escape
/// (GLib rejects these) is kept literally rather than failing the whole file.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The user's locales in preference order, in the forms GKeyFile matches
/// translated keys against - GLib's `g_get_language_names()` without the
/// trailing `C`. Reads `LANGUAGE`, then `LC_ALL`, `LC_MESSAGES` and `LANG`.
pub fn language_names() -> Vec<String> {
    language_names_from(|name| std::env::var(name).ok())
}

fn language_names_from(env: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let value = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(|name| env(name).filter(|v| !v.is_empty()));
    let mut names = Vec::new();
    for locale in value.iter().flat_map(|v| v.split(':')) {
        for variant in locale_variants(locale) {
            if !names.contains(&variant) {
                names.push(variant);
            }
        }
    }
    names
}

/// `lang_COUNTRY.CODESET@MODIFIER` -> `lang_COUNTRY@MODIFIER`,
/// `lang_COUNTRY`, `lang@MODIFIER`, `lang` (the codeset never appears in
/// key-file translations). `C` and `POSIX` have no translations.
fn locale_variants(locale: &str) -> Vec<String> {
    let (rest, modifier) = match locale.split_once('@') {
        Some((rest, modifier)) => (rest, Some(modifier)),
        None => (locale, None),
    };
    let rest = rest.split_once('.').map_or(rest, |(before, _)| before);
    let (lang, country) = match rest.split_once('_') {
        Some((lang, country)) => (lang, Some(country)),
        None => (rest, None),
    };
    if lang.is_empty() || lang == "C" || lang == "POSIX" {
        return Vec::new();
    }
    let mut variants = Vec::new();
    if let Some(country) = country {
        if let Some(modifier) = modifier {
            variants.push(format!("{lang}_{country}@{modifier}"));
        }
        variants.push(format!("{lang}_{country}"));
    }
    if let Some(modifier) = modifier {
        variants.push(format!("{lang}@{modifier}"));
    }
    variants.push(lang.to_string());
    variants
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "\
# a comment
[Data Source]
DisplayName=Personal
DisplayName[de]=Persönlich
DisplayName[pt_BR]=Pessoal
Enabled=true
Parent=local-stub

[Calendar]
BackendName = local
Color=#f5c211
Selected=false

[Data Source]
Enabled=false
";

    #[test]
    fn reads_groups_and_entries() {
        let file = KeyFile::parse(SOURCE).unwrap();
        assert!(file.has_group("Data Source"));
        assert!(file.has_group("Calendar"));
        assert!(!file.has_group("Address Book"));
        assert_eq!(file.get("Data Source", "DisplayName"), Some("Personal"));
        assert_eq!(file.get("Calendar", "Color"), Some("#f5c211"));
        assert_eq!(file.get("Calendar", "Missing"), None);
        assert_eq!(file.get("Missing", "DisplayName"), None);
    }

    #[test]
    fn whitespace_around_the_equals_sign_is_trimmed() {
        let file = KeyFile::parse(SOURCE).unwrap();
        assert_eq!(file.get("Calendar", "BackendName"), Some("local"));
    }

    #[test]
    fn repeated_groups_merge_and_the_last_value_wins() {
        let file = KeyFile::parse(SOURCE).unwrap();
        assert_eq!(file.get_bool("Data Source", "Enabled"), Some(false));
        assert_eq!(file.get("Data Source", "Parent"), Some("local-stub"));
    }

    #[test]
    fn booleans_use_glib_spellings() {
        let file = KeyFile::parse("[G]\na=true\nb=false\nc=1\nd=0\ne=yes\nf=True\n").unwrap();
        assert_eq!(file.get_bool("G", "a"), Some(true));
        assert_eq!(file.get_bool("G", "b"), Some(false));
        assert_eq!(file.get_bool("G", "c"), Some(true));
        assert_eq!(file.get_bool("G", "d"), Some(false));
        assert_eq!(file.get_bool("G", "e"), None);
        assert_eq!(file.get_bool("G", "f"), None);
    }

    #[test]
    fn empty_values_are_absent_for_get_non_empty() {
        let file = KeyFile::parse("[Data Source]\nParent=\n").unwrap();
        assert_eq!(file.get("Data Source", "Parent"), Some(""));
        assert_eq!(file.get_non_empty("Data Source", "Parent"), None);
    }

    #[test]
    fn values_are_unescaped() {
        let file = KeyFile::parse("[G]\nk=\\sleading\\tand\\nmore\\\\done\\x\n").unwrap();
        assert_eq!(file.get("G", "k"), Some(" leading\tand\nmore\\done\\x"));
    }

    #[test]
    fn translated_values_follow_locale_preference() {
        let file = KeyFile::parse(SOURCE).unwrap();
        let get = |locales: &[&str]| {
            let locales: Vec<String> = locales.iter().map(|l| l.to_string()).collect();
            file.get_locale_string("Data Source", "DisplayName", &locales).map(str::to_string)
        };
        assert_eq!(get(&["de_AT", "de"]).as_deref(), Some("Persönlich"));
        assert_eq!(get(&["pt_BR", "pt"]).as_deref(), Some("Pessoal"));
        assert_eq!(get(&["fr_FR", "fr"]).as_deref(), Some("Personal"));
        assert_eq!(get(&[]).as_deref(), Some("Personal"));
    }

    #[test]
    fn malformed_files_are_rejected() {
        assert_eq!(KeyFile::parse("Key=before group\n[G]\n").unwrap_err(), KeyFileError::EntryOutsideGroup(1));
        assert_eq!(KeyFile::parse("[G]\nno equals sign\n").unwrap_err(), KeyFileError::InvalidLine(2));
        assert_eq!(KeyFile::parse("[G\n").unwrap_err(), KeyFileError::InvalidLine(1));
        assert_eq!(KeyFile::parse("[G]\n=value\n").unwrap_err(), KeyFileError::InvalidLine(2));
        assert!(KeyFile::parse("").unwrap().groups.is_empty());
    }

    #[test]
    fn locale_variants_match_glib() {
        assert_eq!(locale_variants("de_DE.UTF-8@euro"), ["de_DE@euro", "de_DE", "de@euro", "de"]);
        assert_eq!(locale_variants("pt_BR.UTF-8"), ["pt_BR", "pt"]);
        assert_eq!(locale_variants("sr@latin"), ["sr@latin", "sr"]);
        assert_eq!(locale_variants("fr"), ["fr"]);
        assert!(locale_variants("C.UTF-8").is_empty());
        assert!(locale_variants("POSIX").is_empty());
    }

    #[test]
    fn language_names_read_the_first_set_variable() {
        let env = |vars: &'static [(&'static str, &'static str)]| move |name: &str| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string());
        assert_eq!(language_names_from(env(&[("LANGUAGE", "en_AU:de"), ("LANG", "fr_FR.UTF-8")])), ["en_AU", "en", "de"]);
        assert_eq!(language_names_from(env(&[("LANGUAGE", ""), ("LANG", "fr_FR.UTF-8")])), ["fr_FR", "fr"]);
        assert_eq!(language_names_from(env(&[("LC_ALL", "C.UTF-8"), ("LANG", "fr_FR.UTF-8")])), Vec::<String>::new());
        assert_eq!(language_names_from(env(&[])), Vec::<String>::new());
    }
}
