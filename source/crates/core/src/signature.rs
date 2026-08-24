/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
///
/// A user-authored rich-text signature, stored alongside the sending
/// identities in the local `AppConfig`. Signatures are global - not pinned
/// to any account - so a future insertion step can pick one per message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Signature {
    pub id: uuid::Uuid,
    pub name: String,
    /// The rich-text form, read back from the editor's contenteditable
    /// document at save time.
    pub html: String,
    /// The plain-text rendering of the same content, kept so a signature can
    /// be appended to a text-mode message without another conversion.
    pub text: String,
}

impl Signature {
    pub fn new(name: impl Into<String>, html: impl Into<String>, text: impl Into<String>) -> Self {
        Signature {
            id: uuid::Uuid::new_v4(),
            name: name.into(),
            html: html.into(),
            text: text.into(),
        }
    }
}
