use lookout_dav::session::CalendarCredentialProvider;
use lookout_dav::Credential;
use lookout_goa::{CalendarAuthMethod, GoaCalendarAccount, GoaClient};

/// Bridges GOA account discovery to `lookout-dav`'s `CalendarCredentialProvider`
/// trait, keeping `lookout-dav` itself free of any D-Bus/GOA dependency.
/// Mirrors `GoaCredentialProvider` (the Mail equivalent).
pub struct GoaCalendarCredentialProvider {
    client: GoaClient,
    account: GoaCalendarAccount,
}

impl GoaCalendarCredentialProvider {
    pub fn new(client: GoaClient, account: GoaCalendarAccount) -> Self {
        GoaCalendarCredentialProvider { client, account }
    }
}

#[async_trait::async_trait]
impl CalendarCredentialProvider for GoaCalendarCredentialProvider {
    async fn calendar_credential(&self) -> Result<Credential, String> {
        self.client.ensure_credentials_calendar(&self.account).await.map_err(|e| e.to_string())?;
        match &self.account.auth {
            CalendarAuthMethod::OAuth2 => {
                let (token, _expires_in) = self.client.get_access_token_calendar(&self.account).await.map_err(|e| e.to_string())?;
                Ok(Credential::OAuth2AccessToken(token))
            }
            CalendarAuthMethod::Password { .. } => {
                let password = self.client.get_calendar_password(&self.account).await.map_err(|e| e.to_string())?;
                Ok(Credential::Password(password))
            }
        }
    }
}
