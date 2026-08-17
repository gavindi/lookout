/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use async_imap::Authenticator;

/// SASL XOAUTH2 (used by Google, Microsoft, and other OAuth2 mail providers).
/// The initial response is sent unprompted on the first continuation
/// request; if the server rejects it and issues a further challenge
/// (carrying a JSON error payload per Google's XOAUTH2 spec), we respond
/// with an empty string to complete the failed handshake instead of
/// re-sending credentials and looping.
pub struct XOAuth2Authenticator {
    user: String,
    access_token: String,
    sent_response: bool,
}

impl XOAuth2Authenticator {
    pub fn new(user: impl Into<String>, access_token: impl Into<String>) -> Self {
        XOAuth2Authenticator {
            user: user.into(),
            access_token: access_token.into(),
            sent_response: false,
        }
    }
}

impl Authenticator for XOAuth2Authenticator {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        if self.sent_response {
            String::new()
        } else {
            self.sent_response = true;
            format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.access_token)
        }
    }
}
