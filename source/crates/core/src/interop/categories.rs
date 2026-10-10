//! Outlook's 25 category colour presets (OUTLOOK_PARITY.md §4.4, UX-85).
//!
//! Microsoft Graph names these `preset0`…`preset24` and documents the *names*
//! (Red, Orange, Brown, … DarkCranberry) but deliberately publishes no hex
//! values: the rendered colour is stated to be client-dependent. The `rgb`
//! column below is therefore a **captured reference** - the current Outlook
//! 365 / OWA rendering, in picker order - not an official value, and should be
//! confirmed against a captured sample when the fixture corpus lands (0.8).
//! The Graph `presetN` identifier and the name are the stable interface;
//! `nearest_preset_for_rgb` is the tolerant bridge for importing a colour the
//! user already has (a `$Lookout-tag-*` tag, or another client's palette) onto
//! the closest preset.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
/// One Outlook category colour preset: its Graph identifier, its official
/// (legacy) name, and the reference RGB capture.
///
/// The name is the value Graph and Office add-ins use; it does not necessarily
/// describe the rendered hue (the new 365 palette kept the old names), which is
/// why the RGB is carried separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CategoryPreset {
    /// 0–24, matching the `presetN` Graph constant.
    pub index: u8,
    /// Graph's constant, e.g. `"preset7"`.
    pub graph: &'static str,
    /// Graph's official name, e.g. `"Blue"`.
    pub name: &'static str,
    /// Reference RGB in `#rrggbb` form (captured, not published by Microsoft).
    pub rgb: &'static str,
}

/// The 25 presets in Graph/picker order.
///
/// `name` is Graph's official label; `rgb` is the current Outlook 365 / OWA
/// rendering captured for that index. Microsoft publishes no hex values, so
/// treat the RGB as a reference to be re-verified against a live build (0.8).
pub const OUTLOOK_CATEGORY_PRESETS: [CategoryPreset; 25] = [
    CategoryPreset {
        index: 0,
        graph: "preset0",
        name: "Red",
        rgb: "#DC626D",
    },
    CategoryPreset {
        index: 1,
        graph: "preset1",
        name: "Orange",
        rgb: "#E8825D",
    },
    CategoryPreset {
        index: 2,
        graph: "preset2",
        name: "Brown",
        rgb: "#FFCD8F",
    },
    CategoryPreset {
        index: 3,
        graph: "preset3",
        name: "Yellow",
        rgb: "#FDEE65",
    },
    CategoryPreset {
        index: 4,
        graph: "preset4",
        name: "Green",
        rgb: "#52CE90",
    },
    CategoryPreset {
        index: 5,
        graph: "preset5",
        name: "Teal",
        rgb: "#57D2DA",
    },
    CategoryPreset {
        index: 6,
        graph: "preset6",
        name: "Olive",
        rgb: "#B6D767",
    },
    CategoryPreset {
        index: 7,
        graph: "preset7",
        name: "Blue",
        rgb: "#5CA9E5",
    },
    CategoryPreset {
        index: 8,
        graph: "preset8",
        name: "Purple",
        rgb: "#B1AAEB",
    },
    CategoryPreset {
        index: 9,
        graph: "preset9",
        name: "Cranberry",
        rgb: "#EE5FB7",
    },
    CategoryPreset {
        index: 10,
        graph: "preset10",
        name: "Steel",
        rgb: "#C5CED1",
    },
    CategoryPreset {
        index: 11,
        graph: "preset11",
        name: "DarkSteel",
        rgb: "#4497A9",
    },
    CategoryPreset {
        index: 12,
        graph: "preset12",
        name: "Gray",
        rgb: "#C3C5BB",
    },
    CategoryPreset {
        index: 13,
        graph: "preset13",
        name: "DarkGray",
        rgb: "#9FADB1",
    },
    CategoryPreset {
        index: 14,
        graph: "preset14",
        name: "Black",
        rgb: "#8F8F8F",
    },
    CategoryPreset {
        index: 15,
        graph: "preset15",
        name: "DarkRed",
        rgb: "#AC4E5E",
    },
    CategoryPreset {
        index: 16,
        graph: "preset16",
        name: "DarkOrange",
        rgb: "#DF8E64",
    },
    CategoryPreset {
        index: 17,
        graph: "preset17",
        name: "DarkBrown",
        rgb: "#BC8F6F",
    },
    CategoryPreset {
        index: 18,
        graph: "preset18",
        name: "DarkYellow",
        rgb: "#DAC257",
    },
    CategoryPreset {
        index: 19,
        graph: "preset19",
        name: "DarkGreen",
        rgb: "#4CA64C",
    },
    CategoryPreset {
        index: 20,
        graph: "preset20",
        name: "DarkTeal",
        rgb: "#4BB4B7",
    },
    CategoryPreset {
        index: 21,
        graph: "preset21",
        name: "DarkOlive",
        rgb: "#85B44C",
    },
    CategoryPreset {
        index: 22,
        graph: "preset22",
        name: "DarkBlue",
        rgb: "#4179A3",
    },
    CategoryPreset {
        index: 23,
        graph: "preset23",
        name: "DarkPurple",
        rgb: "#A589CB",
    },
    CategoryPreset {
        index: 24,
        graph: "preset24",
        name: "DarkCranberry",
        rgb: "#C34E98",
    },
];

/// The preset for a Graph colour constant, e.g. `"preset7"` →
/// [`OUTLOOK_CATEGORY_PRESETS`]`[7]`. Matching is case-insensitive; `"none"`
/// (Graph's no-colour value) and anything unrecognised yield `None`.
pub fn preset_by_graph(value: &str) -> Option<&'static CategoryPreset> {
    let value = value.trim();
    OUTLOOK_CATEGORY_PRESETS.iter().find(|preset| preset.graph.eq_ignore_ascii_case(value))
}

/// Graph's `presetN` constant for an index, or `None` when it is out of range.
pub fn graph_preset(index: u8) -> Option<&'static str> {
    OUTLOOK_CATEGORY_PRESETS.get(usize::from(index)).map(|preset| preset.graph)
}

/// The preset whose captured RGB is closest to `rgb` (Euclidean distance in
/// RGB space). Accepts `#rrggbb` and bare `rrggbb`, upper- or lower-case.
/// Unparseable input falls back to the first preset (`Red`) rather than
/// failing, since this is a cosmetic mapping.
pub fn nearest_preset_for_rgb(rgb: &str) -> &'static CategoryPreset {
    let Some((r, g, b)) = parse_rgb(rgb) else {
        return &OUTLOOK_CATEGORY_PRESETS[0];
    };
    let mut best = &OUTLOOK_CATEGORY_PRESETS[0];
    let mut best_distance = u32::MAX;
    for preset in &OUTLOOK_CATEGORY_PRESETS {
        let Some((pr, pg, pb)) = parse_rgb(preset.rgb) else {
            continue;
        };
        let dr = i32::from(r) - i32::from(pr);
        let dg = i32::from(g) - i32::from(pg);
        let db = i32::from(b) - i32::from(pb);
        let distance = (dr * dr + dg * dg + db * db) as u32;
        if distance < best_distance {
            best_distance = distance;
            best = preset;
        }
    }
    best
}

/// Parses `#rrggbb` / `rrggbb` into its three bytes.
fn parse_rgb(value: &str) -> Option<(u8, u8, u8)> {
    let hex = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_are_exactly_25_presets_in_index_order() {
        assert_eq!(OUTLOOK_CATEGORY_PRESETS.len(), 25);
        for (position, preset) in OUTLOOK_CATEGORY_PRESETS.iter().enumerate() {
            assert_eq!(usize::from(preset.index), position);
            assert_eq!(preset.graph, format!("preset{position}"));
        }
    }

    #[test]
    fn graph_round_trips_through_lookup() {
        for preset in &OUTLOOK_CATEGORY_PRESETS {
            assert_eq!(preset_by_graph(preset.graph), Some(preset));
            assert_eq!(graph_preset(preset.index), Some(preset.graph));
        }
        assert_eq!(preset_by_graph("PRESET7"), OUTLOOK_CATEGORY_PRESETS.get(7));
        assert_eq!(preset_by_graph("none"), None);
        assert_eq!(preset_by_graph("preset25"), None);
        assert_eq!(graph_preset(25), None);
    }

    #[test]
    fn each_preset_rgb_is_valid_and_unique() {
        for preset in &OUTLOOK_CATEGORY_PRESETS {
            assert!(parse_rgb(preset.rgb).is_some(), "bad rgb for {}", preset.graph);
        }
        let mut seen: Vec<&str> = OUTLOOK_CATEGORY_PRESETS.iter().map(|p| p.rgb).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), OUTLOOK_CATEGORY_PRESETS.len());
    }

    #[test]
    fn exact_colours_match_their_own_preset() {
        for preset in &OUTLOOK_CATEGORY_PRESETS {
            assert_eq!(nearest_preset_for_rgb(preset.rgb), preset);
        }
    }

    #[test]
    fn nearest_match_is_tolerant_of_case_hash_and_small_drift() {
        assert_eq!(nearest_preset_for_rgb("#dc626d"), OUTLOOK_CATEGORY_PRESETS.first().unwrap());
        assert_eq!(nearest_preset_for_rgb("5CA9E5"), OUTLOOK_CATEGORY_PRESETS.get(7).unwrap());
        // A colour just off preset 7 (Blue) still resolves to it, not to a neighbour.
        assert_eq!(nearest_preset_for_rgb("#5CA8E4"), OUTLOOK_CATEGORY_PRESETS.get(7).unwrap());
    }

    #[test]
    fn unparseable_colour_falls_back_to_the_first_preset() {
        assert_eq!(nearest_preset_for_rgb("not-a-colour").index, 0);
        assert_eq!(nearest_preset_for_rgb("#12345").index, 0);
    }
}
