//! Category definitions - Outlook's shared category master list (item 2.1 /
//! M-09, CAL-07, T-08).
//!
//! A category is a client-side `{key, name, preset, shortcut}` definition,
//! shared by mail, events and tasks. The `key` is derived once from the name
//! and never changed by a rename: on a message it anchors the IMAP keyword
//! `$Lookout-tag-<key>` (see `lookout_core::tag_keyword`), and on an event or
//! task it is matched case-insensitively against the iCalendar `CATEGORIES`
//! name. The colour is always one of Outlook's 25 presets
//! (`lookout_core::OUTLOOK_CATEGORY_PRESETS`, §4.4 / UX-85), never a raw hex -
//! so a category is portable to/from Microsoft 365's Graph
//! `masterCategories` by its preset index.
//!
//! The list lives in `settings.json` (`app_config::AppConfig::categories`),
//! not a separate file. A pre-2.1 `tags.json` is migrated once at startup
//! (`migrate_legacy_tags`), mapping each raw colour onto the nearest preset.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use lookout_core::{nearest_preset_for_rgb, preset_by_index, CategoryPreset, OUTLOOK_CATEGORY_PRESETS};

use crate::app_config::config_dir;

/// The number of function keys classic Outlook binds category shortcuts to:
/// `Ctrl+F2` … `Ctrl+F12` (11 keys).
pub const SHORTCUT_KEYS: std::ops::RangeInclusive<u8> = 2..=12;

/// One defined category.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryDef {
    /// The stable key, embedded in the message keyword as
    /// `$Lookout-tag-<key>`. Derived once from the name at creation and
    /// never changed by a rename, so keywords already stored on the server
    /// keep matching.
    pub key: String,
    /// The user-facing name ("Work", "Project Xmas"), matched case-insensitively
    /// against an event/task's iCalendar `CATEGORIES`.
    pub name: String,
    /// The Outlook colour preset index, `0`–`24` (Graph's `presetN`).
    pub preset: u8,
    /// The classic category shortcut's function-key number (`2`–`12`), or
    /// `None`. `2` renders as `Ctrl+F2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shortcut: Option<u8>,
}

impl CategoryDef {
    /// A new category with no shortcut assigned.
    pub fn new(key: impl Into<String>, name: impl Into<String>, preset: u8) -> Self {
        CategoryDef {
            key: key.into(),
            name: name.into(),
            preset,
            shortcut: None,
        }
    }

    /// The preset data (name/Graph constant/RGB) for this category, falling
    /// back to the first preset if the stored index is out of range.
    pub fn preset(&self) -> &'static CategoryPreset {
        preset_by_index(self.preset).unwrap_or(&OUTLOOK_CATEGORY_PRESETS[0])
    }

    /// The category's colour as `#rrggbb`, from its preset.
    pub fn color(&self) -> &'static str {
        self.preset().rgb
    }

    /// This category's message keyword, `$Lookout-tag-<key>`.
    pub fn keyword(&self) -> String {
        lookout_core::tag_keyword(&self.key)
    }
}

/// The ordered set of defined categories. Order is user-visible (the
/// Categorize menu and the editor both render in list order).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CategorySet {
    #[serde(default)]
    pub categories: Vec<CategoryDef>,
}

impl CategorySet {
    /// Builds the set from the persisted list.
    pub fn from_defs(categories: Vec<CategoryDef>) -> Self {
        CategorySet { categories }
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.categories.iter().any(|c| c.key == key)
    }

    /// The category whose name matches `name` case-insensitively (trimmed).
    pub fn find_by_name(&self, name: &str) -> Option<&CategoryDef> {
        let name = name.trim();
        self.categories.iter().find(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// Whether a category with this name already exists.
    pub fn contains_name(&self, name: &str) -> bool {
        self.find_by_name(name).is_some()
    }

    /// The category assigned to `shortcut`, if any.
    pub fn by_shortcut(&self, shortcut: u8) -> Option<&CategoryDef> {
        self.categories.iter().find(|c| c.shortcut == Some(shortcut))
    }
}

/// The configured categories a message's keywords refer to, in definition
/// order. Keywords outside the `$Lookout-tag-*` namespace are ignored.
pub fn categories_for_keywords<'a>(categories: &'a CategorySet, keywords: &BTreeSet<String>) -> Vec<&'a CategoryDef> {
    categories.categories.iter().filter(|c| keywords.contains(&c.keyword())).collect()
}

/// The preset a brand-new category gets: successive adds walk the 25 presets
/// round-robin, so a fresh list is colourful without the user choosing.
pub fn default_preset(index: usize) -> u8 {
    (index % OUTLOOK_CATEGORY_PRESETS.len()) as u8
}

/// `$XDG_CONFIG_HOME/lookout/tags.json` - the pre-2.1 tag file, read once for
/// migration and no longer written.
pub fn legacy_tags_path() -> PathBuf {
    config_dir().join("lookout").join("tags.json")
}

/// A pre-2.1 tag row (`{key, name, color}`), colour being an arbitrary hex.
#[derive(Debug, Deserialize)]
struct LegacyTag {
    key: String,
    name: String,
    #[serde(default)]
    color: String,
}

#[derive(Debug, Deserialize)]
struct LegacyTagSet {
    #[serde(default)]
    tags: Vec<LegacyTag>,
}

/// Reads the pre-2.1 `tags.json`, mapping each raw colour onto the nearest
/// Outlook preset, so a user's existing tags survive the move into
/// `settings.json`. Returns `None` when the file is absent or unreadable (the
/// normal case for a fresh install) - never an error.
pub fn migrate_legacy_tags() -> Option<Vec<CategoryDef>> {
    migrate_legacy_tags_at(&legacy_tags_path())
}

/// The path-taking form of [`migrate_legacy_tags`], so tests can point it at
/// a temp file instead of the process-global `XDG_CONFIG_HOME`.
fn migrate_legacy_tags_at(path: &std::path::Path) -> Option<Vec<CategoryDef>> {
    let text = std::fs::read_to_string(path).ok()?;
    let legacy: LegacyTagSet = serde_json::from_str(&text).ok()?;
    if legacy.tags.is_empty() {
        return None;
    }
    let categories = legacy
        .tags
        .into_iter()
        .map(|tag| CategoryDef {
            key: tag.key,
            name: tag.name,
            preset: nearest_preset_for_rgb(&tag.color).index,
            shortcut: None,
        })
        .collect();
    Some(categories)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> CategorySet {
        CategorySet::from_defs(vec![
            CategoryDef::new("work", "Work", 7),
            CategoryDef {
                key: "project-xmas".into(),
                name: "Project Xmas".into(),
                preset: 0,
                shortcut: Some(2),
            },
        ])
    }

    #[test]
    fn color_and_preset_resolve_and_clamp() {
        let work = CategoryDef::new("work", "Work", 7);
        assert_eq!(work.color(), "#5CA9E5");
        assert_eq!(work.preset().name, "Blue");
        // Out-of-range index degrades to the first preset, never panics.
        let bad = CategoryDef::new("bad", "Bad", 99);
        assert_eq!(bad.color(), OUTLOOK_CATEGORY_PRESETS[0].rgb);
    }

    #[test]
    fn categories_match_by_key_and_name_case_insensitively() {
        let set = set();
        assert!(set.contains_key("work"));
        assert!(set.contains_name("project xmas"));
        assert!(set.contains_name("Project Xmas"));
        assert_eq!(set.find_by_name("WORK").unwrap().key, "work");
        assert!(set.find_by_name("Unknown").is_none());
    }

    #[test]
    fn keywords_select_categories_in_definition_order() {
        let set = set();
        let mut keywords = BTreeSet::new();
        keywords.insert(lookout_core::tag_keyword("project-xmas"));
        keywords.insert(lookout_core::tag_keyword("work"));
        keywords.insert("$Other-junk".into());
        let matched = categories_for_keywords(&set, &keywords);
        assert_eq!(matched.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), vec!["work", "project-xmas"]);
    }

    #[test]
    fn shortcut_lookup_and_default_preset_wrap() {
        let set = set();
        assert_eq!(set.by_shortcut(2).unwrap().key, "project-xmas");
        assert!(set.by_shortcut(3).is_none());
        assert_eq!(default_preset(0), 0);
        assert_eq!(default_preset(25), 0);
        assert_eq!(default_preset(26), 1);
    }

    #[test]
    fn legacy_tags_migrate_to_nearest_presets() {
        let dir = std::env::temp_dir().join(format!("lookout-categories-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tags.json");

        // Missing file -> nothing to migrate.
        assert!(migrate_legacy_tags_at(&path).is_none());

        std::fs::write(
            &path,
            r##"{"tags":[{"key":"work","name":"Work","color":"#5CA9E5"},{"key":"red","name":"Important","color":"#DC626D"}]}"##,
        )
        .unwrap();
        let migrated = migrate_legacy_tags_at(&path).expect("tags.json should migrate");
        assert_eq!(migrated.len(), 2);
        assert_eq!(migrated[0].preset, 7, "the Blue hex maps to preset 7");
        assert_eq!(migrated[0].shortcut, None);
        assert_eq!(migrated[1].preset, 0);

        std::fs::write(&path, "not json").unwrap();
        assert!(migrate_legacy_tags_at(&path).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
