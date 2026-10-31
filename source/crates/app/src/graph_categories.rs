//! Syncs the category master list with Outlook on Microsoft 365 accounts
//! through Microsoft Graph (item 2.1 / M-09, CAL-07, T-08; §4.4).
//!
//! Microsoft 365 keeps its categories as `/me/outlook/masterCategories`
//! (`{ id, displayName, color: presetN }`). Graph is the only way to see or
//! change them for an M365 account, so this client pulls them on connect and
//! pushes local edits back, keyed by name (the shared master list identifies a
//! category by its name, and a name maps 1:1 onto a Graph `masterCategory`).
//!
//! Both directions only run once the account has granted Graph access
//! (Config's "Sync with Outlook"), the same gate as pin and flag mirroring.
//! The colour is one of Outlook's 25 presets, so it round-trips through
//! `lookout_core::{preset_by_graph, graph_preset}` (§4.4, UX-85).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use lookout_core::{graph_preset, preset_by_graph};
use lookout_graph::GraphError;
use serde_json::{json, Value};

use crate::graph_account::{GraphAccount, GraphFeatureClient};

/// One remote category: its Graph id, its name, and its preset index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphCategory {
    pub id: String,
    pub name: String,
    pub preset: u8,
}

/// One Microsoft 365 account's Graph category client.
pub struct GraphCategoriesClient {
    account: GraphAccount,
}

impl GraphCategoriesClient {
    pub fn new(account: GraphAccount) -> Self {
        GraphCategoriesClient { account }
    }

    /// Every category in the mailbox's master list.
    pub async fn list(&self) -> Result<Vec<GraphCategory>, GraphError> {
        let items = self.account.graph().get_all("me/outlook/masterCategories?$top=100", 500).await?;
        Ok(items.iter().filter_map(category_from_graph).collect())
    }

    /// Creates a category. `Err(GraphError::Status{409,..})` when one with the
    /// same name already exists.
    pub async fn create(&self, name: &str, preset: u8) -> Result<(), GraphError> {
        self.account.graph().post("me/outlook/masterCategories", &category_body(name, preset)).await?;
        Ok(())
    }

    /// Repoints an existing category (id) to `name`/`preset`.
    pub async fn update(&self, id: &str, name: &str, preset: u8) -> Result<(), GraphError> {
        self.account
            .graph()
            .patch(&format!("me/outlook/masterCategories/{id}"), &category_body(name, preset))
            .await?;
        Ok(())
    }
}

impl GraphFeatureClient for GraphCategoriesClient {
    fn account(&self) -> &GraphAccount {
        &self.account
    }
}

/// The Graph request body for a category: its name and preset colour.
pub fn category_body(name: &str, preset: u8) -> Value {
    json!({ "displayName": name, "color": graph_preset(preset).unwrap_or("preset0") })
}

/// Parses a Graph `masterCategory`. `None` for a malformed entry (no id or no
/// display name); a missing/unknown colour falls back to the first preset.
pub fn category_from_graph(value: &Value) -> Option<GraphCategory> {
    let id = value.get("id")?.as_str()?.to_string();
    let name = value.get("displayName")?.as_str()?.to_string();
    let preset = value.get("color").and_then(Value::as_str).and_then(preset_by_graph).map(|p| p.index).unwrap_or(0);
    Some(GraphCategory { id, name, preset })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_uses_the_graph_preset_name() {
        assert_eq!(category_body("Work", 7), json!({ "displayName": "Work", "color": "preset7" }));
        // An out-of-range index can't fail the request: it falls back to preset0.
        assert_eq!(category_body("Odd", 99), json!({ "displayName": "Odd", "color": "preset0" }));
    }

    #[test]
    fn parses_a_graph_category() {
        let cat = category_from_graph(&json!({ "id": "c1", "displayName": "Work", "color": "preset7" })).unwrap();
        assert_eq!(
            cat,
            GraphCategory {
                id: "c1".into(),
                name: "Work".into(),
                preset: 7
            }
        );
        // Missing colour defaults to the first preset; a missing id/name is None.
        let plain = category_from_graph(&json!({ "id": "c2", "displayName": "No colour" })).unwrap();
        assert_eq!(plain.preset, 0);
        assert!(category_from_graph(&json!({ "displayName": "No id" })).is_none());
        assert!(category_from_graph(&json!({ "id": "c3" })).is_none());
    }

    #[test]
    fn reads_a_captured_master_categories_response() {
        let page: Value = serde_json::from_str(include_str!("../../../test-fixtures/outlook/master-categories.json")).unwrap();
        let categories: Vec<GraphCategory> = lookout_graph::page_items(&page).iter().filter_map(category_from_graph).collect();
        assert_eq!(categories.len(), 5);
        assert_eq!(categories[2].name, "Work");
        assert_eq!(categories[2].preset, 7);
        assert_eq!(categories[3].preset, 24);
        // The colour-less entry falls back to the first preset.
        assert_eq!(categories[4].preset, 0);
    }
}
