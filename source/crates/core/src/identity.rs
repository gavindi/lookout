use crate::email::EmailAddress;
use crate::ids::AccountId;

/// Purely local sending-identity configuration tied to an SMTP `MAIL FROM`
/// override — unlike Bulwark's JMAP `Identity`, there is no server-stored
/// object to sync, since IMAP/SMTP have no such concept.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Identity {
    pub id: uuid::Uuid,
    pub account_id: AccountId,
    pub name: String,
    pub email: String,
    pub reply_to: Vec<EmailAddress>,
    pub bcc: Vec<EmailAddress>,
    pub text_signature: Option<String>,
    pub html_signature: Option<String>,
}

impl Identity {
    pub fn new(account_id: AccountId, name: impl Into<String>, email: impl Into<String>) -> Self {
        Identity {
            id: uuid::Uuid::new_v4(),
            account_id,
            name: name.into(),
            email: email.into(),
            reply_to: Vec::new(),
            bcc: Vec::new(),
            text_signature: None,
            html_signature: None,
        }
    }

    /// A dropdown label: `Name <email>` when a display name is set, the bare
    /// address otherwise.
    pub fn label(&self) -> String {
        let name = self.name.trim();
        if name.is_empty() {
            self.email.clone()
        } else {
            format!("{name} <{}>", self.email)
        }
    }
}
