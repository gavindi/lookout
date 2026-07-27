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

/// Rustls 0.23 refuses to pick a default `CryptoProvider` automatically when
/// more than one backend is linked into the binary - which happens here
/// because `lettre`'s `tokio1-rustls-tls` feature pulls in `aws-lc-rs`
/// alongside whatever our own TLS stack resolves to, and panics on the
/// first TLS handshake instead. Installing one explicitly, once, up front
/// resolves it process-wide (both our own IMAP/SMTP TLS connections use the
/// same rustls-wide default). Called from both `connect_tls` (IMAP) and
/// `send::send_smtp`, guarded by `Once` so it's harmless to call from both.
pub(crate) fn ensure_crypto_provider_installed() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

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
    ensure_crypto_provider_installed();

    // Test-only escape hatch so lookout-mail's own integration tests (see
    // tests/imap_integration.rs) can talk to an ephemeral local GreenMail
    // container's self-signed certificate. Doubly gated: the `test-utils`
    // Cargo feature only exists in builds of this crate's own test targets
    // (enabled via a self-referencing [dev-dependencies] entry - see
    // Cargo.toml), so this branch is compiled out entirely for any real
    // consumer (lookout-app, or lookout-mail used as a library elsewhere),
    // and even within a test-utils build it requires deliberately setting
    // an unusual, clearly-named env var to activate.
    #[cfg(feature = "test-utils")]
    if std::env::var_os("LOOKOUT_INSECURE_TLS_FOR_TESTS").is_some() {
        tracing::warn!("LOOKOUT_INSECURE_TLS_FOR_TESTS set: skipping TLS certificate verification");
        return connect_tls_insecure_for_tests(host, port).await;
    }

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

#[cfg(feature = "test-utils")]
async fn connect_tls_insecure_for_tests(host: &str, port: u16) -> Result<ImapStream> {
    use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use tokio_rustls::rustls::crypto::CryptoProvider;
    use tokio_rustls::rustls::pki_types::{CertificateDer, UnixTime};
    use tokio_rustls::rustls::{DigitallySignedStruct, SignatureScheme};

    /// Accepts any server certificate and any signature without checking
    /// them at all. Only ever compiled into this crate's own test targets
    /// (see the doc comment on the `#[cfg(feature = "test-utils")]` call
    /// site above) - never link this into anything that talks to a real
    /// mail server.
    #[derive(Debug)]
    struct AcceptAnyCert(std::sync::Arc<CryptoProvider>);

    impl ServerCertVerifier for AcceptAnyCert {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &tokio_rustls::rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> std::result::Result<ServerCertVerified, tokio_rustls::rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    let provider = std::sync::Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
    let config = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("aws-lc-rs supports rustls' default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(AcceptAnyCert(provider)))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));

    let tcp = TcpStream::connect((host, port)).await?;
    let server_name = ServerName::try_from(host.to_string()).map_err(|_| Error::InvalidServerName(host.to_string()))?;
    let tls = connector.connect(server_name, tcp).await?;
    Ok(tls)
}
