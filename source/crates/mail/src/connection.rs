use std::sync::Arc;

use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::{client::TlsStream, TlsConnector};

use crate::error::{Error, Result};

/// The stream type handed to `async_imap::Client::new()`. With the crate's
/// `runtime-tokio` feature enabled, async-imap's internal `Read`/`Write`
/// bounds are `tokio::io::AsyncRead`/`AsyncWrite` directly (see
/// `async_imap::client`'s `#[cfg(feature = "runtime-tokio")] use tokio::io::
/// {AsyncRead as Read, AsyncWrite as Write}`) - a native tokio-rustls stream
/// can be handed over as-is, with no `futures-io` compat shim needed.
pub type ImapStream = TlsStream<TcpStream>;

fn root_cert_store() -> Result<RootCertStore> {
    let mut store = RootCertStore::empty();
    let result = rustls_native_certs::load_native_certs();
    for err in result.errors {
        tracing::warn!("error loading a native root certificate: {err}");
    }
    for cert in result.certs {
        // Individual malformed certs are skipped rather than failing the
        // whole connection; a single bad system cert shouldn't block mail.
        let _ = store.add(cert);
    }
    if store.is_empty() {
        return Err(Error::NoRootCertificates);
    }
    Ok(store)
}

/// Opens a TLS connection to `host:port` suitable for wrapping in an
/// `async_imap::Client` or driving an SMTP `AUTH`/submission session.
pub async fn connect_tls(host: &str, port: u16) -> Result<ImapStream> {
    tracing::debug!("connect_tls: building root store");
    let root_store = root_cert_store()?;
    let config = ClientConfig::builder().with_root_certificates(root_store).with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));

    tracing::debug!("connect_tls: tcp connecting to {host}:{port}");
    let tcp = TcpStream::connect((host, port)).await?;
    tracing::debug!("connect_tls: tcp connected, starting tls handshake");
    let server_name = ServerName::try_from(host.to_string()).map_err(|_| Error::InvalidServerName(host.to_string()))?;
    let tls = connector.connect(server_name, tcp).await?;
    tracing::debug!("connect_tls: tls handshake complete");
    Ok(tls)
}
