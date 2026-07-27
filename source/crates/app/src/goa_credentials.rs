use lookout_goa::{AuthMethod, GoaClient, GoaMailAccount};
use lookout_mail::session::CredentialProvider;
use lookout_mail::Credential;

/// Bridges GOA account discovery to `lookout-mail`'s `CredentialProvider`
/// trait, keeping `lookout-mail` itself free of any D-Bus/GOA dependency.
pub struct GoaCredentialProvider {
    client: GoaClient,
    account: GoaMailAccount,
}

impl GoaCredentialProvider {
    pub fn new(client: GoaClient, account: GoaMailAccount) -> Self {
        GoaCredentialProvider { client, account }
    }
}

#[async_trait::async_trait]
impl CredentialProvider for GoaCredentialProvider {
    async fn imap_credential(&self) -> Result<Credential, String> {
        self.client.ensure_credentials(&self.account).await.map_err(|e| e.to_string())?;
        match &self.account.auth {
            AuthMethod::OAuth2 => {
                let (token, _expires_in) =
                    self.client.get_access_token(&self.account).await.map_err(|e| e.to_string())?;
                Ok(Credential::OAuth2AccessToken(token))
            }
            AuthMethod::Password { .. } => {
                let password = self.client.get_imap_password(&self.account).await.map_err(|e| e.to_string())?;
                Ok(Credential::Password(password))
            }
        }
    }
}
