//! Manual verification tool: prints every GOA account with Mail enabled and
//! fetches a live credential for each, exercising both the OAuth2 and
//! password-based code paths against whatever accounts are actually
//! configured on this machine. Run with `cargo run -p lookout-goa --example list_accounts`.

use lookout_goa::{AuthMethod, GoaClient};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = GoaClient::connect().await?;
    let accounts = client.list_mail_accounts().await?;

    if accounts.is_empty() {
        println!("No GOA accounts with Mail enabled found.");
        return Ok(());
    }

    for account in &accounts {
        println!("--- {} <{}> ---", account.display_name, account.email);
        println!("  path: {}", account.object_path);
        println!(
            "  imap: {}:{} tls={} user={}",
            account.imap.host,
            account.imap.port.map(|p| p.to_string()).unwrap_or_else(|| "default".into()),
            account.imap.use_tls,
            account.imap.username
        );
        println!(
            "  smtp: {}:{} tls={} user={}",
            account.smtp.host,
            account.smtp.port.map(|p| p.to_string()).unwrap_or_else(|| "default".into()),
            account.smtp.use_tls,
            account.smtp.username
        );

        match client.ensure_credentials(account).await {
            Ok(()) => println!("  ensure_credentials: ok"),
            Err(e) => {
                println!("  ensure_credentials: FAILED ({e}) - skipping credential fetch");
                continue;
            }
        }

        match &account.auth {
            AuthMethod::OAuth2 => match client.get_access_token(account).await {
                Ok((token, expires_in)) => {
                    println!("  auth: OAuth2, got access token ({} chars, expires in {}s)", token.len(), expires_in);
                }
                Err(e) => println!("  auth: OAuth2, FAILED to get access token: {e}"),
            },
            AuthMethod::Password { .. } => match client.get_imap_password(account).await {
                Ok(pw) => println!("  auth: Password, got IMAP password ({} chars)", pw.len()),
                Err(e) => println!("  auth: Password, FAILED to get IMAP password: {e}"),
            },
        }
    }

    Ok(())
}
