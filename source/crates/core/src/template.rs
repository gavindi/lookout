/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
///
/// A user-authored rich-text "My Templates" snippet (OUTLOOK_PARITY.md item
/// 2.10 / C-17), inserted into the composer from the template menu. Stored
/// alongside the sending identities and signatures in the local `AppConfig`.
/// Templates are global - not pinned to any account.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MailTemplate {
    pub id: uuid::Uuid,
    pub name: String,
    /// The rich-text form, read back from the editor's contenteditable
    /// document at save time.
    pub html: String,
    /// The plain-text rendering of the same content, kept so a template can be
    /// inserted into a text-mode message without another conversion.
    pub text: String,
}

impl MailTemplate {
    pub fn new(name: impl Into<String>, html: impl Into<String>, text: impl Into<String>) -> Self {
        MailTemplate {
            id: uuid::Uuid::new_v4(),
            name: name.into(),
            html: html.into(),
            text: text.into(),
        }
    }
}
