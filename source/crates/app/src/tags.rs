//! Tag definitions for message color-tags.
//!
//! A tag is a client-side `{key, name, color}` definition. Assigning a tag to
//! a message stores the IMAP keyword `$Lookout-tag-<key>` on it server-side
//! (see `lookout_core::tag_keyword`); this module only owns the definition
//! set - the name a user reads, the key that anchors the keyword, and the
//! color drawn on message rows. Persisted as a plain JSON file
//! (`$XDG_CONFIG_HOME/lookout/tags.json`), the same best-effort convention as
//! calendar colours: a missing or unreadable file just means no tags, never an
//! error.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::calendar_colors::CALENDAR_PALETTE;

/// One defined color tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagDef {
    /// The stable key, embedded in the message keyword as
    /// `$Lookout-tag-<key>`. Derived once from the tag name at creation and
    /// never changed by a rename, so keywords already stored on the server
    /// keep matching.
    pub key: String,
    /// The user-facing name ("Work", "Project Xmas").
    pub name: String,
    /// Hex color ("#rrggbb").
    pub color: String,
}

/// The ordered set of defined tags. Order is user-visible (the Categorize
/// menu and the Manage dialog both render in list order).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TagSet {
    pub tags: Vec<TagDef>,
}

impl TagSet {
    pub fn contains_key(&self, key: &str) -> bool {
        self.tags.iter().any(|t| t.key == key)
    }
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var_os("HOME").unwrap_or_else(|| std::env::var_os("USERPROFILE").unwrap_or_default());
        PathBuf::from(home).join(".config")
    })
}

/// `$XDG_CONFIG_HOME/lookout/tags.json` (or the equivalent `~/.config` path
/// when `XDG_CONFIG_HOME` is unset).
pub fn tags_path() -> PathBuf {
    config_dir().join("lookout").join("tags.json")
}

/// Loads the saved tag set. Any failure (missing file, bad JSON, unreadable
/// dir) yields an empty set - tags are cosmetic, so a broken file should
/// never take the app down.
pub fn load() -> TagSet {
    match std::fs::read_to_string(tags_path()) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!(path = %tags_path().display(), "ignoring unreadable tags.json: {e}");
            TagSet::default()
        }),
        Err(_) => TagSet::default(),
    }
}

/// Writes the tag set out (creating the directory as needed). Best-effort: a
/// read-only home or disk error only logs a warning.
pub fn save(tags: &TagSet) {
    let path = tags_path();
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(path = %path.display(), "could not create config dir: {e}");
            return;
        }
    }
    match serde_json::to_string_pretty(tags) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&path, text) {
                tracing::warn!(path = %path.display(), "could not save tags: {e}");
            }
        }
        Err(e) => tracing::warn!("could not serialize tags: {e}"),
    }
}

/// The configured tags a message's keywords refer to, in definition order.
/// Keywords outside the `$Lookout-tag-*` namespace are ignored.
pub fn tags_for_keywords<'a>(tags: &'a TagSet, keywords: &BTreeSet<String>) -> Vec<&'a TagDef> {
    tags.tags.iter().filter(|t| keywords.contains(&lookout_core::tag_keyword(&t.key))).collect()
}

/// The color to draw for a brand-new tag: the palette color at `index`,
/// wrapping round-robin, skipping nothing. The dialog picks `index` = current
/// tag count, so successive tags get distinct colors until the palette wraps.
pub fn default_tag_color(index: usize) -> &'static str {
    CALENDAR_PALETTE[index % CALENDAR_PALETTE.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    // A single test owns `XDG_CONFIG_HOME`: the env var is process-global and
    // parallel test threads would race over it otherwise (see
    // `background_image`'s tests for the same rule).
    #[test]
    fn config_file_round_trip_and_tolerance() {
        let dir = std::env::temp_dir().join(format!("lookout-tags-test-{}", std::process::id()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Missing file -> empty set.
        assert_eq!(load().tags.len(), 0);

        let set = TagSet {
            tags: vec![
                TagDef {
                    key: "work".into(),
                    name: "Work".into(),
                    color: "#3584e4".into(),
                },
                TagDef {
                    key: "project-xmas".into(),
                    name: "Project Xmas".into(),
                    color: "#e01b24".into(),
                },
            ],
        };
        save(&set);
        let loaded = load();
        assert_eq!(loaded, set);
        assert_eq!(loaded.tags[0].name, "Work");

        // A broken file also reads back as empty, never an error.
        std::fs::create_dir_all(dir.join("lookout")).unwrap();
        std::fs::write(tags_path(), "not json").unwrap();
        assert_eq!(load().tags.len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tags_for_keywords_matches_only_ours() {
        let tags = TagSet {
            tags: vec![
                TagDef {
                    key: "work".into(),
                    name: "Work".into(),
                    color: "#3584e4".into(),
                },
                TagDef {
                    key: "red".into(),
                    name: "Important".into(),
                    color: "#e01b24".into(),
                },
            ],
        };
        let mut keywords = BTreeSet::new();
        keywords.insert(lookout_core::tag_keyword("work"));
        keywords.insert("$Other-tag-junk".into());
        let matched = tags_for_keywords(&tags, &keywords);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].key, "work");
    }

    #[test]
    fn default_colors_wrap_round_robin() {
        assert_eq!(default_tag_color(0), CALENDAR_PALETTE[0]);
        assert_eq!(default_tag_color(CALENDAR_PALETTE.len()), CALENDAR_PALETTE[0]);
    }
}
