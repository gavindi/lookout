//! Syncs the user's rules with Outlook on Microsoft 365 accounts through
//! Microsoft Graph `messageRules` (item 2.5 / M-14; §4.4).
//!
//! Microsoft 365 keeps inbox rules at `/me/mailFolders/inbox/messageRules`.
//! Graph is the only way to see or change them for an M365 account, so this
//! client pulls them on connect and pushes local `Graph`-backend rules back.
//! A Graph rule is always tied to one account (its mailbox owns the rule), so
//! the account id travels on [`lookout_core::Rule::account`], and the Graph id
//! on [`lookout_core::Rule::id`] so updates and deletes can target it.
//!
//! Graph rules can't express everything the shared model can: a follow-up
//! `Flag` action and "has no attachment" have no counterpart, and a
//! move/copy target must be a folder the account actually has (Graph wants a
//! folder id, not a name). [`rule_to_graph`] returns `None` for such a rule,
//! and the sync leaves it as a client rule rather than silently dropping part
//! of it. Reads are best-effort: an unrepresentable Graph condition/action is
//! simply skipped when mapping back.
//!
//! Both directions only run once the account has granted Graph access
//! (Config's "Sync with Outlook"), the same gate as pin, flag and category
//! sync. The endpoint needs `MailboxSettings.ReadWrite`, already in
//! `GRAPH_SCOPES` and `GraphFeature::MailboxSettings`.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::sync::Arc;

use lookout_core::{AccountId, EmailAddress, Importance, MailboxRole, Rule, RuleAction, RuleBackend, RuleCondition, RuleTarget, SizeComparison};
use lookout_graph::{GraphClient, GraphError};
use serde_json::{json, Map, Value};

use crate::microsoft_oauth::MicrosoftOAuth;

/// One Microsoft 365 folder, enough to map a move/copy target to a Graph
/// folder id and back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphFolder {
    pub id: String,
    pub display_name: String,
    /// Graph's `wellKnownName` ("inbox", "archive", "deleteditems", …), when
    /// the folder is one of the well-known ones.
    pub well_known_name: Option<String>,
}

/// One Microsoft 365 account's Graph rules client.
pub struct GraphRulesClient {
    oauth: Arc<MicrosoftOAuth>,
    graph: GraphClient,
    account_id: AccountId,
}

impl GraphRulesClient {
    pub fn new(account_id: AccountId) -> Self {
        let oauth = Arc::new(MicrosoftOAuth::new(account_id.clone()));
        GraphRulesClient {
            graph: GraphClient::new(oauth.clone()),
            oauth,
            account_id,
        }
    }

    /// Whether the account has granted Graph access (disk-only check).
    pub fn consented(&self) -> bool {
        self.oauth.graph_consented()
    }

    /// Every inbox rule, mapped back into the shared model (backend `Graph`,
    /// account set, id = Graph id).
    pub async fn list(&self) -> Result<Vec<Rule>, GraphError> {
        let folders = self.folders().await?;
        let items = self.graph.get_all("me/mailFolders/inbox/messageRules?$top=100", 500).await?;
        Ok(items.iter().filter_map(|value| rule_from_graph(value, &folders, &self.account_id)).collect())
    }

    /// Creates `rule` and returns its Graph id.
    pub async fn create(&self, rule: &Rule) -> Result<String, GraphError> {
        let folders = self.folders().await?;
        let body = rule_to_graph(rule, &folders).ok_or_else(unexpressible)?;
        let created = self.graph.post("me/mailFolders/inbox/messageRules", &body).await?;
        Ok(created.get("id").and_then(Value::as_str).unwrap_or_default().to_string())
    }

    /// Repoints the existing Graph rule (`rule.id`) at `rule`.
    pub async fn update(&self, rule: &Rule) -> Result<(), GraphError> {
        let folders = self.folders().await?;
        let body = rule_to_graph(rule, &folders).ok_or_else(unexpressible)?;
        self.graph.patch(&format!("me/mailFolders/inbox/messageRules/{}", rule.id), &body).await?;
        Ok(())
    }

    /// Deletes the Graph rule with `id`.
    pub async fn delete(&self, id: &str) -> Result<(), GraphError> {
        self.graph.delete(&format!("me/mailFolders/inbox/messageRules/{id}")).await?;
        Ok(())
    }

    /// The account's mailbox folders (for move/copy target resolution).
    pub async fn folders(&self) -> Result<Vec<GraphFolder>, GraphError> {
        let items = self.graph.get_all("me/mailFolders?$top=250&$select=id,displayName,wellKnownName", 500).await?;
        Ok(items.iter().filter_map(folder_from_graph).collect())
    }
}

/// The error a rule that Graph can't express produces, so the sync can skip it
/// (status `0` distinguishes it from a real HTTP status).
fn unexpressible() -> GraphError {
    GraphError::Status {
        status: 0,
        message: "This rule uses conditions or actions Microsoft 365 rules can't express".to_string(),
        request_id: None,
    }
}

/// Parses a Graph `mailFolder`.
pub fn folder_from_graph(value: &Value) -> Option<GraphFolder> {
    let id = value.get("id")?.as_str()?.to_string();
    let display_name = value.get("displayName").and_then(Value::as_str).unwrap_or_default().to_string();
    let well_known_name = value.get("wellKnownName").and_then(Value::as_str).map(str::to_string);
    Some(GraphFolder {
        id,
        display_name,
        well_known_name,
    })
}

/// The Graph `wellKnownName` for a role, when it has one.
fn well_known_for_role(role: MailboxRole) -> Option<&'static str> {
    match role {
        MailboxRole::Inbox => Some("inbox"),
        MailboxRole::Sent => Some("sentitems"),
        MailboxRole::Drafts => Some("drafts"),
        MailboxRole::Trash => Some("deleteditems"),
        MailboxRole::Junk => Some("junkemail"),
        MailboxRole::Archive => Some("archive"),
        MailboxRole::Custom => None,
    }
}

/// Resolves a move/copy target to a Graph folder id.
fn folder_id_for(target: &RuleTarget, folders: &[GraphFolder]) -> Option<String> {
    match target {
        RuleTarget::Role(role) => {
            let well_known = well_known_for_role(*role)?;
            folders
                .iter()
                .find(|folder| folder.well_known_name.as_deref() == Some(well_known))
                .map(|folder| folder.id.clone())
        }
        RuleTarget::Folder(name) => folders.iter().find(|folder| folder.display_name.eq_ignore_ascii_case(name)).map(|folder| folder.id.clone()),
    }
}

/// The inverse: a folder id back to a role (for well-known folders) or a named
/// folder.
fn target_for_folder(id: &str, folders: &[GraphFolder]) -> RuleTarget {
    let Some(folder) = folders.iter().find(|folder| folder.id == id) else {
        return RuleTarget::Folder(id.to_string());
    };
    match folder.well_known_name.as_deref() {
        Some("inbox") => RuleTarget::Role(MailboxRole::Inbox),
        Some("sentitems") => RuleTarget::Role(MailboxRole::Sent),
        Some("drafts") => RuleTarget::Role(MailboxRole::Drafts),
        Some("deleteditems") => RuleTarget::Role(MailboxRole::Trash),
        Some("junkemail") => RuleTarget::Role(MailboxRole::Junk),
        Some("archive") => RuleTarget::Role(MailboxRole::Archive),
        _ => RuleTarget::Folder(folder.display_name.clone()),
    }
}

/// Appends `value` to a string array field, creating it if absent.
fn push_str(map: &mut Map<String, Value>, key: &str, value: &str) {
    let entry = map.entry(key.to_string()).or_insert_with(|| Value::Array(Vec::new()));
    if let Value::Array(items) = entry {
        items.push(Value::String(value.to_string()));
    }
}

/// The header line a `Header` condition maps to ("X-Spam: YES"); an empty
/// name maps to just the value.
fn header_line(name: &str, value: &str) -> String {
    if name.trim().is_empty() {
        value.to_string()
    } else {
        format!("{name}: {value}")
    }
}

/// Maps the shared conditions into a Graph `messageRulePredicates` object, or
/// `None` when one can't be expressed (Graph has no "has no attachment").
pub fn conditions_to_graph(conditions: &[RuleCondition]) -> Option<Value> {
    let mut map = Map::new();
    for condition in conditions {
        match condition {
            RuleCondition::From(text) => push_str(&mut map, "senderContains", text),
            RuleCondition::To(text) | RuleCondition::Cc(text) => push_str(&mut map, "recipientContains", text),
            RuleCondition::Subject(text) => push_str(&mut map, "subjectContains", text),
            RuleCondition::Body(text) => push_str(&mut map, "bodyContains", text),
            RuleCondition::Header { name, value } => push_str(&mut map, "headerContains", &header_line(name, value)),
            RuleCondition::ListId(text) => push_str(&mut map, "headerContains", text),
            RuleCondition::HasAttachment(true) => {
                map.insert("hasAttachments".to_string(), Value::Bool(true));
            }
            RuleCondition::HasAttachment(false) => return None,
            RuleCondition::Importance(importance) => {
                map.insert("importance".to_string(), Value::String(importance.as_graph_str().to_string()));
            }
            RuleCondition::Size { comparison, bytes } => {
                let mut range = Map::new();
                match comparison {
                    SizeComparison::GreaterThan | SizeComparison::GreaterOrEqual => {
                        range.insert("minimumSize".to_string(), json!(bytes));
                    }
                    SizeComparison::LessThan | SizeComparison::LessOrEqual => {
                        range.insert("maximumSize".to_string(), json!(bytes));
                    }
                    SizeComparison::Equal => {
                        range.insert("minimumSize".to_string(), json!(bytes));
                        range.insert("maximumSize".to_string(), json!(bytes));
                    }
                }
                map.insert("withinSizeRange".to_string(), Value::Object(range));
            }
            RuleCondition::SentOnlyToMe => {
                map.insert("sentOnlyToMe".to_string(), Value::Bool(true));
            }
        }
    }
    Some(Value::Object(map))
}

/// Maps a list of addresses to Graph recipients.
fn recipients(addresses: &[EmailAddress]) -> Value {
    Value::Array(
        addresses
            .iter()
            .map(|address| json!({ "emailAddress": { "address": address.address, "name": address.name } }))
            .collect(),
    )
}

/// Maps the shared actions into a Graph `messageRuleActions` object, or `None`
/// when one can't be expressed (a follow-up `Flag`, or an unresolved
/// move/copy folder).
pub fn actions_to_graph(actions: &[RuleAction], folders: &[GraphFolder]) -> Option<Value> {
    let mut map = Map::new();
    for action in actions {
        match action {
            RuleAction::Move(target) => {
                map.insert("moveToFolder".to_string(), Value::String(folder_id_for(target, folders)?));
            }
            RuleAction::Copy(target) => {
                map.insert("copyToFolder".to_string(), Value::String(folder_id_for(target, folders)?));
            }
            RuleAction::Delete => {
                map.insert("delete".to_string(), Value::Bool(true));
            }
            RuleAction::Flag(_) => return None,
            RuleAction::Categorize(names) => {
                map.insert("assignCategories".to_string(), json!(names));
            }
            RuleAction::MarkRead(read) => {
                map.insert("markAsRead".to_string(), Value::Bool(*read));
            }
            RuleAction::Forward(to) => {
                map.insert("forwardTo".to_string(), recipients(to));
            }
            RuleAction::Redirect(to) => {
                map.insert("redirectTo".to_string(), recipients(to));
            }
            RuleAction::StopProcessing => {
                map.insert("stopProcessingRules".to_string(), Value::Bool(true));
            }
        }
    }
    Some(Value::Object(map))
}

/// Maps a rule to a Graph `messageRule` body, or `None` when it can't be
/// expressed on Microsoft 365.
pub fn rule_to_graph(rule: &Rule, folders: &[GraphFolder]) -> Option<Value> {
    let conditions = conditions_to_graph(&rule.conditions)?;
    let actions = actions_to_graph(&rule.actions, folders)?;
    let mut body = json!({
        "displayName": rule.name,
        "sequence": 1,
        "isEnabled": rule.enabled,
        "conditions": conditions,
        "actions": actions,
    });
    if !rule.exceptions.is_empty() {
        body["exceptions"] = conditions_to_graph(&rule.exceptions)?;
    }
    Some(body)
}

/// Maps a Graph `messageRulePredicates` back into shared conditions,
/// best-effort (unsupported predicates are skipped).
pub fn conditions_from_graph(value: Option<&Value>, folders: &[GraphFolder]) -> Vec<RuleCondition> {
    let _ = folders;
    let Some(conditions) = value else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for text in string_array(conditions, "senderContains") {
        out.push(RuleCondition::From(text));
    }
    for text in string_array(conditions, "recipientContains") {
        out.push(RuleCondition::To(text));
    }
    for text in string_array(conditions, "subjectContains") {
        out.push(RuleCondition::Subject(text));
    }
    for text in string_array(conditions, "bodyContains") {
        out.push(RuleCondition::Body(text));
    }
    for text in string_array(conditions, "headerContains") {
        out.push(RuleCondition::Header { name: String::new(), value: text });
    }
    if conditions.get("hasAttachments").and_then(Value::as_bool) == Some(true) {
        out.push(RuleCondition::HasAttachment(true));
    }
    if let Some(importance) = conditions.get("importance").and_then(Value::as_str).and_then(Importance::from_graph) {
        out.push(RuleCondition::Importance(importance));
    }
    if let Some(range) = conditions.get("withinSizeRange") {
        if let Some(minimum) = range.get("minimumSize").and_then(Value::as_u64) {
            out.push(RuleCondition::Size {
                comparison: SizeComparison::GreaterOrEqual,
                bytes: minimum,
            });
        }
        if let Some(maximum) = range.get("maximumSize").and_then(Value::as_u64) {
            out.push(RuleCondition::Size {
                comparison: SizeComparison::LessOrEqual,
                bytes: maximum,
            });
        }
    }
    if conditions.get("sentOnlyToMe").and_then(Value::as_bool) == Some(true) {
        out.push(RuleCondition::SentOnlyToMe);
    }
    out
}

/// Maps a Graph `messageRuleActions` back into shared actions, best-effort.
pub fn actions_from_graph(value: Option<&Value>, folders: &[GraphFolder]) -> Vec<RuleAction> {
    let Some(actions) = value else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(id) = actions.get("moveToFolder").and_then(Value::as_str) {
        out.push(RuleAction::Move(target_for_folder(id, folders)));
    }
    if let Some(id) = actions.get("copyToFolder").and_then(Value::as_str) {
        out.push(RuleAction::Copy(target_for_folder(id, folders)));
    }
    if actions.get("delete").and_then(Value::as_bool) == Some(true) || actions.get("permanentDelete").and_then(Value::as_bool) == Some(true) {
        out.push(RuleAction::Delete);
    }
    if let Some(categories) = actions.get("assignCategories") {
        let names = string_array_value(categories);
        if !names.is_empty() {
            out.push(RuleAction::Categorize(names));
        }
    }
    if let Some(read) = actions.get("markAsRead").and_then(Value::as_bool) {
        out.push(RuleAction::MarkRead(read));
    }
    let to = recipients_from(actions.get("forwardTo"));
    if !to.is_empty() {
        out.push(RuleAction::Forward(to));
    }
    let to = recipients_from(actions.get("redirectTo"));
    if !to.is_empty() {
        out.push(RuleAction::Redirect(to));
    }
    if actions.get("stopProcessingRules").and_then(Value::as_bool) == Some(true) {
        out.push(RuleAction::StopProcessing);
    }
    out
}

/// Maps a Graph `messageRule` back into the shared model.
pub fn rule_from_graph(value: &Value, folders: &[GraphFolder], account: &AccountId) -> Option<Rule> {
    let id = value.get("id")?.as_str()?.to_string();
    let name = value.get("displayName").and_then(Value::as_str).unwrap_or("Rule").to_string();
    let enabled = value.get("isEnabled").and_then(Value::as_bool).unwrap_or(true);
    Some(Rule {
        id,
        name,
        enabled,
        backend: RuleBackend::Graph,
        account: Some(account.clone()),
        conditions: conditions_from_graph(value.get("conditions"), folders),
        exceptions: conditions_from_graph(value.get("exceptions"), folders),
        actions: actions_from_graph(value.get("actions"), folders),
    })
}

fn string_array(value: &Value, key: &str) -> Vec<String> {
    value.get(key).map(string_array_value).unwrap_or_default()
}

fn string_array_value(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn recipients_from(value: Option<&Value>) -> Vec<EmailAddress> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let address = item.get("emailAddress")?.get("address")?.as_str()?.to_string();
            let name = item.get("emailAddress").and_then(|e| e.get("name")).and_then(Value::as_str).map(str::to_string);
            Some(EmailAddress { name, address })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folders() -> Vec<GraphFolder> {
        vec![
            GraphFolder {
                id: "f-inbox".into(),
                display_name: "Inbox".into(),
                well_known_name: Some("inbox".into()),
            },
            GraphFolder {
                id: "f-archive".into(),
                display_name: "Archive".into(),
                well_known_name: Some("archive".into()),
            },
            GraphFolder {
                id: "f-deleted".into(),
                display_name: "Deleted Items".into(),
                well_known_name: Some("deleteditems".into()),
            },
            GraphFolder {
                id: "f-projects".into(),
                display_name: "Projects".into(),
                well_known_name: None,
            },
        ]
    }

    #[test]
    fn maps_move_and_conditions_to_graph() {
        let mut rule = Rule::new("", "Archive newsletters");
        rule.account = Some(AccountId("acct".into()));
        rule.conditions = vec![RuleCondition::From("news@example.com".into()), RuleCondition::ListId("list.example.com".into())];
        rule.exceptions = vec![RuleCondition::From("vip@example.com".into())];
        rule.actions = vec![
            RuleAction::Categorize(vec!["Newsletters".into()]),
            RuleAction::MarkRead(true),
            RuleAction::Move(RuleTarget::Role(MailboxRole::Archive)),
        ];
        let body = rule_to_graph(&rule, &folders()).expect("expressible");
        assert_eq!(body["displayName"], "Archive newsletters");
        assert_eq!(body["conditions"]["senderContains"][0], "news@example.com");
        assert_eq!(body["conditions"]["headerContains"][0], "list.example.com");
        assert_eq!(body["exceptions"]["senderContains"][0], "vip@example.com");
        assert_eq!(body["actions"]["moveToFolder"], "f-archive");
        assert_eq!(body["actions"]["markAsRead"], true);
        assert_eq!(body["actions"]["assignCategories"][0], "Newsletters");
    }

    #[test]
    fn flag_and_no_attachment_are_not_expressible() {
        let mut rule = Rule::new("", "Flag it");
        rule.actions = vec![RuleAction::Flag(lookout_core::FollowUpFlag {
            status: lookout_core::FlagStatus::Flagged,
            ..lookout_core::FollowUpFlag::default()
        })];
        assert!(rule_to_graph(&rule, &folders()).is_none());
        let mut rule = Rule::new("", "No attachment");
        rule.conditions = vec![RuleCondition::HasAttachment(false)];
        assert!(rule_to_graph(&rule, &folders()).is_none());
        // A move to a folder the account doesn't have is also unexpressible.
        let mut rule = Rule::new("", "Move weird");
        rule.actions = vec![RuleAction::Move(RuleTarget::Folder("Nonexistent".into()))];
        assert!(rule_to_graph(&rule, &folders()).is_none());
    }

    #[test]
    fn round_trips_a_captured_graph_rule() {
        let page: Value = serde_json::from_str(include_str!("../../../test-fixtures/outlook/message-rules.json")).unwrap();
        let rules: Vec<Rule> = lookout_graph::page_items(&page)
            .iter()
            .filter_map(|value| rule_from_graph(value, &folders(), &AccountId("acct".into())))
            .collect();
        assert_eq!(rules.len(), 2);
        let first = &rules[0];
        assert_eq!(first.name, "Archive newsletters");
        assert_eq!(first.backend, RuleBackend::Graph);
        assert_eq!(first.account, Some(AccountId("acct".into())));
        assert!(first.conditions.iter().any(|c| matches!(c, RuleCondition::From(f) if f == "news@example.com")));
        // `headerContains` has no header name, so it maps back to an
        // any-header condition.
        assert!(first
            .conditions
            .iter()
            .any(|c| matches!(c, RuleCondition::Header { name, value } if name.is_empty() && value == "list.example.com")));
        assert!(first.actions.iter().any(|a| matches!(a, RuleAction::Move(RuleTarget::Role(MailboxRole::Archive)))));
        assert!(first
            .actions
            .iter()
            .any(|a| matches!(a, RuleAction::Categorize(names) if names == &vec!["Newsletters".to_string()])));
        // The second rule is disabled and moves to a named folder.
        assert!(!rules[1].enabled);
        assert!(rules[1]
            .actions
            .iter()
            .any(|a| matches!(a, RuleAction::Move(RuleTarget::Folder(name)) if name == "Projects")));
    }

    #[test]
    fn role_and_folder_targets_resolve() {
        assert_eq!(folder_id_for(&RuleTarget::Role(MailboxRole::Archive), &folders()).as_deref(), Some("f-archive"));
        assert_eq!(folder_id_for(&RuleTarget::Folder("projects".into()), &folders()).as_deref(), Some("f-projects"));
        assert_eq!(folder_id_for(&RuleTarget::Folder("nope".into()), &folders()), None);
        assert_eq!(target_for_folder("f-deleted", &folders()), RuleTarget::Role(MailboxRole::Trash));
        assert_eq!(target_for_folder("f-projects", &folders()), RuleTarget::Folder("Projects".into()));
    }
}
