//! The Microsoft Graph contacts transport: the HTTP operations the People
//! screen's sync loop drives, layered on [`GraphClient`] (OUTLOOK_PARITY.md
//! §5.2, items 5.2 / A-04, P-11).
//!
//! Every method returns `lookout_core` types through the pure mapping in the
//! parent module, so the sync loop never touches Graph JSON. `@odata.nextLink`
//! paging is handled by [`GraphClient::get_all`]; an incremental sync passes a
//! previously returned delta cursor to [`ContactsClient::contacts_delta`],
//! whose `DeltaPage::delta_link` the caller persists. Directory lookups
//! (`/me/people`, `/users?$search=`) and photos (`/photo/$value`) back the
//! GAL and profile-card photos.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use lookout_core::text::percent_encode;
use lookout_core::VCard;

use crate::client::{page_items, DeltaPage, GraphClient};
use crate::error::{GraphError, Result};
use crate::scopes::GraphFeature;

use super::{contact_folder_from_graph, contact_from_graph, contact_to_graph, person_from_graph, user_from_graph, ContactFolder, DirectoryPerson, GraphContact};

/// Most contact folders read for one account.
const MAX_CONTACT_FOLDERS: usize = 500;
/// Most contacts read for one folder (a guard against an unbounded mailbox).
const MAX_CONTACTS: usize = 20000;
/// Most directory entries returned for one search.
const MAX_DIRECTORY: usize = 50;

/// The `$select` fields the contact read needs - exactly what the mapper
/// consumes, so Graph doesn't return the full contact bodies.
const CONTACT_SELECT: &str = "$select=id,changeKey,parentFolderId,displayName,givenName,middleName,surname,generation,title,jobTitle,initials,nickName,fileAs,companyName,department,officeLocation,profession,manager,assistantName,spouseName,children,emailAddresses,homePhones,businessPhones,mobilePhone,homeAddress,businessAddress,otherAddress,imAddresses,personalNotes,categories,birthday,businessHomePage";

/// The contacts HTTP operations, over a shared [`GraphClient`].
pub struct ContactsClient {
    graph: GraphClient,
}

impl ContactsClient {
    pub fn new(graph: GraphClient) -> Self {
        ContactsClient { graph }
    }

    /// The account's contact folders (`/me/contactFolders`).
    pub async fn list_folders(&self) -> Result<Vec<ContactFolder>> {
        let items = self.graph.get_all("me/contactFolders?$top=100&$select=id,displayName", MAX_CONTACT_FOLDERS).await?;
        Ok(items.iter().filter_map(contact_folder_from_graph).collect())
    }

    /// Every contact in `folder_id` (or the default contacts folder when
    /// `None`).
    pub async fn list_contacts(&self, folder_id: Option<&str>) -> Result<Vec<GraphContact>> {
        let items = self.graph.get_all(&self.contacts_path(folder_id), MAX_CONTACTS).await?;
        Ok(items.iter().filter_map(contact_from_graph).collect())
    }

    /// An incremental contacts sync: pass `None` for the initial read, or a
    /// previously returned [`DeltaPage::delta_link`] for the next one. The
    /// returned link is the caller's to persist. `/delta` tombstones are read
    /// (by the parent module's `removed_id`), so a deletion reaches the cache.
    pub async fn contacts_delta(&self, folder_id: Option<&str>, delta_link: Option<&str>) -> Result<DeltaPage> {
        self.graph.delta(&self.delta_path(folder_id), delta_link).await
    }

    /// Creates `card` in `folder_id` (the default folder when `None`) and
    /// returns the server's copy (with its Graph id/`changeKey`).
    pub async fn create_contact(&self, folder_id: Option<&str>, card: &VCard) -> Result<GraphContact> {
        let path = match folder_id {
            Some(folder) => format!("me/contactFolders/{}/contacts", encode_segment(folder)),
            None => "me/contacts".to_string(),
        };
        let created = self.graph.post(&path, &contact_to_graph(card)).await?;
        contact_from_graph(&created).ok_or_else(malformed_contact)
    }

    /// Updates `card` in place, guarded by `etag` (`If-Match`) so a concurrent
    /// change fails loudly (HTTP 412) rather than being clobbered. `Prefer:
    /// return=representation` asks Graph for the updated resource; if the
    /// server omits the body, the input card is returned unchanged (the sync
    /// loop resyncs after a write anyway).
    pub async fn update_contact(&self, id: &str, etag: Option<&str>, card: &VCard) -> Result<GraphContact> {
        let path = format!("me/contacts/{}", encode_segment(id));
        let mut headers: Vec<(&str, &str)> = vec![("Prefer", "return=representation")];
        if let Some(etag) = etag {
            headers.push(("If-Match", etag));
        }
        let updated = self.graph.patch_with_headers(&path, &contact_to_graph(card), &headers).await?;
        Ok(contact_from_graph(&updated).unwrap_or_else(|| GraphContact {
            id: id.to_string(),
            etag: None,
            card: card.clone(),
        }))
    }

    /// Deletes the contact with Graph `id`, guarded by `etag` (`If-Match`)
    /// when known.
    pub async fn delete_contact(&self, id: &str, etag: Option<&str>) -> Result<()> {
        let path = format!("me/contacts/{}", encode_segment(id));
        let headers: Vec<(&str, &str)> = etag.map(|etag| vec![("If-Match", etag)]).unwrap_or_default();
        self.graph.delete_with_headers(&path, &headers).await?;
        Ok(())
    }

    /// The contact's photo bytes (`/me/contacts/{id}/photo/$value`), or `None`
    /// when the contact has no photo.
    pub async fn contact_photo(&self, id: &str) -> Result<Option<Vec<u8>>> {
        self.graph.get_bytes(&format!("me/contacts/{}/photo/$value", encode_segment(id))).await
    }

    /// A people search against the mailbox's relevance-ranked people list
    /// (`/me/people?$search=`) - the "people I work with" GAL surface.
    pub async fn search_people(&self, query: &str, limit: usize) -> Result<Vec<DirectoryPerson>> {
        let search = format!("\"{}\"", sanitize_search(query));
        let path = format!(
            "me/people?$search={}&$select=id,displayName,scoredEmailAddresses&$top={}",
            percent_encode(&search),
            clamp_limit(limit)
        );
        let page = self.graph.get(&path).await?;
        Ok(page_items(&page).iter().filter_map(person_from_graph).collect())
    }

    /// A directory search against the tenant's user list
    /// (`/users?$search=`), the real GAL. Graph requires
    /// `ConsistencyLevel: eventual` for a `$search` on `/users`. Needs the
    /// `User.ReadBasic.All` scope (`GraphFeature::Directory`).
    pub async fn search_directory(&self, query: &str, limit: usize) -> Result<Vec<DirectoryPerson>> {
        let query = sanitize_search(query);
        let search = format!("\"displayName:{query}\" OR \"mail:{query}\"");
        let path = format!(
            "users?$search={}&$select=id,displayName,mail,userPrincipalName,jobTitle,companyName&$count=true&$top={}",
            percent_encode(&search),
            clamp_limit(limit)
        );
        let page = self.graph.get_with_headers(&path, &[("ConsistencyLevel", "eventual")]).await?;
        Ok(page_items(&page).iter().filter_map(user_from_graph).collect())
    }

    /// A directory user's photo bytes (`/users/{id}/photo/$value`), or `None`
    /// when they have no photo.
    pub async fn user_photo(&self, id: &str) -> Result<Option<Vec<u8>>> {
        self.graph.get_bytes(&format!("users/{}/photo/$value", encode_segment(id))).await
    }

    /// Negotiates consent for the contacts scope (`Contacts.ReadWrite`) before
    /// a first request. A no-op once granted.
    pub async fn ensure_consent(&self) -> Result<()> {
        self.graph.ensure_feature(GraphFeature::Contacts).await
    }

    /// Negotiates consent for the directory scopes (`User.ReadBasic.All`,
    /// `People.Read`), which are deliberately outside the default combined
    /// consent (`GraphFeature::Directory`).
    pub async fn ensure_directory_consent(&self) -> Result<()> {
        self.graph.ensure_feature(GraphFeature::Directory).await
    }

    fn contacts_path(&self, folder_id: Option<&str>) -> String {
        match folder_id {
            Some(folder) => format!("me/contactFolders/{}/contacts?$top=100&{CONTACT_SELECT}", encode_segment(folder)),
            None => format!("me/contacts?$top=100&{CONTACT_SELECT}"),
        }
    }

    fn delta_path(&self, folder_id: Option<&str>) -> String {
        match folder_id {
            Some(folder) => format!("me/contactFolders/{}/contacts/delta?$top=100", encode_segment(folder)),
            None => "me/contacts/delta?$top=100".to_string(),
        }
    }
}

/// A Graph-required bound on a `$top`, clamped to a sane range so a caller
/// can't ask for zero (Graph rejects it) or an unbounded page.
fn clamp_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_DIRECTORY)
}

/// Strips characters that would break the `$search` phrase syntax.
fn sanitize_search(query: &str) -> String {
    query.chars().filter(|c| *c != '"' && *c != '\\' && !c.is_control()).collect()
}

/// Percent-encodes a path segment (a Graph contact/folder id), leaving RFC
/// 3986 unreserved characters. Graph ids are base64-ish and may contain `+`,
/// `/` or `=`, which must not leak into the URL path.
fn encode_segment(segment: &str) -> String {
    percent_encode(segment)
}

/// A malformed create/update response (status `0` distinguishes it from a
/// real HTTP status, as in `graph_rules`).
fn malformed_contact() -> GraphError {
    GraphError::Status {
        status: 0,
        message: "Graph returned a contact without an id".to_string(),
        request_id: None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::{json, Value};
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::TokenProvider;
    use crate::error::Result as GraphResult;

    struct StubTokens;

    #[async_trait]
    impl TokenProvider for StubTokens {
        async fn graph_token(&self) -> GraphResult<String> {
            Ok("stub-token".to_string())
        }
        async fn ensure_scopes(&self, _scopes: &[&str]) -> GraphResult<()> {
            Ok(())
        }
    }

    fn client(server: &MockServer) -> ContactsClient {
        ContactsClient::new(GraphClient::with_base_url(Arc::new(StubTokens), format!("{}/v1.0", server.uri())))
    }

    fn contact_json() -> Value {
        json!({
            "id": "c1",
            "changeKey": "ck1",
            "displayName": "Ada King",
            "givenName": "Ada",
            "surname": "King",
            "emailAddresses": [{ "address": "ada@example.com" }]
        })
    }

    #[tokio::test]
    async fn list_folders_maps_the_directory() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/contactFolders"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{ "id": "f1", "displayName": "Work" }, { "id": "f2", "displayName": "Personal" }]
            })))
            .mount(&server)
            .await;

        let folders = client(&server).list_folders().await.unwrap();
        assert_eq!(folders.len(), 2);
        assert_eq!(folders[0].id, "f1");
        assert_eq!(folders[1].display_name, "Personal");
    }

    #[tokio::test]
    async fn list_contacts_maps_every_contact() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/contactFolders/f1/contacts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [contact_json()] })))
            .mount(&server)
            .await;

        let contacts = client(&server).list_contacts(Some("f1")).await.unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].id, "c1");
        assert_eq!(contacts[0].etag.as_deref(), Some("ck1"));
        assert_eq!(contacts[0].card.full_name.as_deref(), Some("Ada King"));
    }

    #[tokio::test]
    async fn contacts_delta_returns_the_cursor() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/contacts/delta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [contact_json()],
                "@odata.deltaLink": "https://graph.microsoft.com/v1.0/delta-cursor"
            })))
            .mount(&server)
            .await;

        let page = client(&server).contacts_delta(None, None).await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.delta_link.as_deref(), Some("https://graph.microsoft.com/v1.0/delta-cursor"));
    }

    #[tokio::test]
    async fn create_contact_posts_and_reads_the_server_copy() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/contactFolders/f1/contacts"))
            .and(body_partial_json(json!({ "displayName": "Ada King" })))
            .respond_with(ResponseTemplate::new(201).set_body_json(contact_json()))
            .mount(&server)
            .await;

        let card = contact_from_graph(&contact_json()).unwrap().card;
        let created = client(&server).create_contact(Some("f1"), &card).await.unwrap();
        assert_eq!(created.id, "c1");
    }

    #[tokio::test]
    async fn update_contact_sends_if_match_and_prefer() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1.0/me/contacts/c1"))
            .and(header("If-Match", "ck1"))
            .and(header("Prefer", "return=representation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "c1",
                "changeKey": "ck2",
                "displayName": "Ada King (moved)"
            })))
            .mount(&server)
            .await;

        let card = contact_from_graph(&contact_json()).unwrap().card;
        let updated = client(&server).update_contact("c1", Some("ck1"), &card).await.unwrap();
        assert_eq!(updated.etag.as_deref(), Some("ck2"));
        assert_eq!(updated.card.full_name.as_deref(), Some("Ada King (moved)"));
    }

    #[tokio::test]
    async fn delete_contact_sends_if_match() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1.0/me/contacts/c1"))
            .and(header("If-Match", "ck1"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        client(&server).delete_contact("c1", Some("ck1")).await.unwrap();
    }

    #[tokio::test]
    async fn contact_photo_returns_bytes_or_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/contacts/c1/photo/$value"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![9u8, 8]))
            .mount(&server)
            .await;
        assert_eq!(client(&server).contact_photo("c1").await.unwrap().as_deref(), Some([9u8, 8].as_slice()));
    }

    #[tokio::test]
    async fn search_people_maps_the_relevance_list() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/people"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{
                    "id": "u1",
                    "displayName": "Ada King",
                    "scoredEmailAddresses": [{ "address": "ada@example.com", "relevanceScore": 1.0 }]
                }]
            })))
            .mount(&server)
            .await;

        let people = client(&server).search_people("ada", 10).await.unwrap();
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].address, "ada@example.com");
        assert_eq!(people[0].name.as_deref(), Some("Ada King"));
    }

    #[tokio::test]
    async fn search_directory_sends_consistency_level_and_maps_users() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/users"))
            .and(header("ConsistencyLevel", "eventual"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{ "id": "u1", "displayName": "Ada King", "mail": "ada@example.com", "jobTitle": "CTO" }]
            })))
            .mount(&server)
            .await;

        let people = client(&server).search_directory("ada", 10).await.unwrap();
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].address, "ada@example.com");
        assert_eq!(people[0].title.as_deref(), Some("CTO"));
    }

    #[test]
    fn encode_segment_escapes_path_specials() {
        assert_eq!(encode_segment("c1"), "c1");
        assert_eq!(encode_segment("a+b/c="), "a%2Bb%2Fc%3D");
    }

    #[test]
    fn sanitize_search_strips_phrase_breakers() {
        assert_eq!(sanitize_search("ada\" OR \"*"), "ada OR *");
    }
}
