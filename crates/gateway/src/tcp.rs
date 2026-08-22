//! TCP connection handling + TLS termination
//! (docs/specs/Networking_Spec.md, "TLS (TCP channel)").
//!
//! Scope: accept a connection, terminate TLS, hand back a stream of
//! decoded [`Envelope`]s. Routing a decoded message to whichever backing
//! service should handle it (auth, chat, ...) is not this crate's job yet
//! — same deferral as chat's gateway integration (#87).

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use common::{Error, Result};
use futures_util::Stream;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_util::codec::Framed;

use crate::envelope::EnvelopeCodec;
use crate::tls::CertMaterial;

/// The workspace links more than one `rustls` crypto provider (`ring` via
/// `rtc-dtls`, `aws-lc-rs` via `sqlx`'s `tls-rustls-aws-lc-rs` feature), so
/// `rustls` can't auto-select one — install `ring` explicitly, matching
/// what `rtc-dtls` itself uses internally. Idempotent; safe to call more
/// than once (e.g. once per test).
pub fn ensure_crypto_provider_installed() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn build_tls_acceptor(cert: &CertMaterial) -> Result<TlsAcceptor> {
    ensure_crypto_provider_installed();

    let cert_chain = vec![CertificateDer::from(cert.cert_der.clone())];
    let key = PrivateKeyDer::try_from(cert.key_der.clone())
        .map_err(|e| Error::new("gateway", format!("invalid private key: {e}")))?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .map_err(|e| Error::wrap("gateway", "failed to build TLS server config", e))?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Binds `addr`, and for each accepted connection performs the TLS
/// handshake and yields a framed envelope stream. A connection that fails
/// its TLS handshake is dropped and logged, not propagated as a listener
/// failure — one bad client shouldn't take down accepting new ones.
pub async fn listen(
    addr: &str,
    acceptor: TlsAcceptor,
) -> Result<(
    SocketAddr,
    impl Stream<Item = Framed<tokio_rustls::server::TlsStream<TcpStream>, EnvelopeCodec>>,
)> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| Error::wrap("gateway", format!("failed to bind {addr}"), e))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| Error::wrap("gateway", "failed to read bound address", e))?;

    let stream = async_stream::stream! {
        loop {
            let (socket, peer_addr) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to accept a TCP connection");
                    continue;
                }
            };

            match acceptor.accept(socket).await {
                Ok(tls_stream) => yield Framed::new(tls_stream, EnvelopeCodec::default()),
                Err(e) => tracing::warn!(%peer_addr, error = %e, "TLS handshake failed"),
            }
        }
    };

    Ok((local_addr, stream))
}

/// Reads a self-signed/configured cert per docs/specs/Networking_Spec.md
/// and logs its fingerprint — the operator-facing "here's what to pin" step.
pub fn init_and_log_fingerprint(config_dir: &Path) -> Result<CertMaterial> {
    let cert = crate::tls::load_or_generate(config_dir)?;
    tracing::info!(fingerprint_sha256 = %cert.fingerprint_sha256_hex, "TLS certificate ready");
    Ok(cert)
}

#[cfg(test)]
mod tests {
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::TlsConnector;
    use tokio_rustls::rustls::RootCertStore;
    use tokio_rustls::rustls::pki_types::ServerName;

    use super::*;
    use crate::envelope::Envelope;

    // Real localhost TCP + a real TLS handshake — no external infra, safe
    // to run in CI. The client trusts the server's self-signed cert
    // directly (added as its own root), matching how an operator would
    // pin the fingerprint logged by `init_and_log_fingerprint` in practice.
    #[tokio::test]
    async fn accepts_a_connection_and_round_trips_a_framed_message() {
        let dir = std::env::temp_dir().join(format!("wz-gateway-tcp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert = crate::tls::load_or_generate(&dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        let acceptor = build_tls_acceptor(&cert).unwrap();
        let (addr, incoming) = listen("127.0.0.1:0", acceptor).await.unwrap();

        let server = tokio::spawn(async move {
            let mut incoming = Box::pin(incoming);
            let mut framed = incoming.next().await.expect("no connection accepted");
            let received = framed
                .next()
                .await
                .expect("stream ended")
                .expect("decode failed");
            framed
                .send(Envelope::new(received.message_type, received.payload))
                .await
                .unwrap();
        });

        let mut roots = RootCertStore::empty();
        roots
            .add(tokio_rustls::rustls::pki_types::CertificateDer::from(
                cert.cert_der.clone(),
            ))
            .unwrap();
        let client_config = tokio_rustls::rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(client_config));

        let tcp = TcpStream::connect(addr).await.unwrap();
        let server_name = ServerName::try_from("localhost").unwrap();
        let mut tls = connector.connect(server_name, tcp).await.unwrap();

        // Hand-encode a frame directly (rather than going through
        // `EnvelopeCodec`) so this test exercises the wire format against
        // an independent implementation, not just against itself.
        let payload = b"ping".to_vec();
        let mut frame = Vec::new();
        frame.extend_from_slice(&(2 + payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&42u16.to_be_bytes());
        frame.extend_from_slice(&payload);
        tls.write_all(&frame).await.unwrap();

        let mut response = vec![0u8; frame.len()];
        tls.read_exact(&mut response).await.unwrap();
        assert_eq!(
            response, frame,
            "server should echo back exactly what was sent"
        );

        server.await.unwrap();
    }
}
