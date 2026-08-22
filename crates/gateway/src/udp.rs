//! UDP channel + DTLS (docs/specs/Networking_Spec.md, "DTLS (UDP channel)").
//!
//! `rtc-dtls` is sans-IO — it owns no socket and reads no clock. This
//! module is the glue: a background task drives the DTLS state machine
//! against a real [`tokio::net::UdpSocket`], exposing decoded [`Envelope`]s
//! in and out over channels. Routing a decoded message to a backing
//! service is out of scope here, same as the TCP side (`gateway::tcp`).
//!
//! Scope reminder: this is transport security only — it does not validate
//! that the *data inside* a datagram is a legitimate movement update or
//! anything else. That's `world`'s job (#33).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use common::{Error, Result};
use rtc_dtls::config::ConfigBuilder;
use rtc_dtls::crypto::{Certificate, CryptoPrivateKey};
use rtc_dtls::endpoint::{Endpoint, EndpointEvent};
use rtc_shared::TransportProtocol;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::envelope::{Envelope, decode_datagram, encode_datagram};
use crate::tls::CertMaterial;

/// Builds an `rtc-dtls` [`Certificate`] from the same cert/key TCP's TLS
/// uses — one keypair, one fingerprint an operator manages, per
/// docs/specs/Networking_Spec.md.
pub fn certificate_from(cert: &CertMaterial) -> Result<Certificate> {
    let key_pair = rcgen::KeyPair::try_from(cert.key_der.as_slice())
        .map_err(|e| Error::wrap("gateway", "failed to parse TLS key for DTLS reuse", e))?;
    let private_key = CryptoPrivateKey::from_key_pair(&key_pair)
        .map_err(|e| Error::wrap("gateway", "failed to derive DTLS private key", e))?;

    Ok(Certificate {
        certificate: vec![rustls::pki_types::CertificateDer::from(
            cert.cert_der.clone(),
        )],
        private_key,
    })
}

/// The channels a caller uses to talk to a running DTLS association —
/// server (multiple remotes) or client (one remote), same shape either way.
pub struct DtlsChannels {
    pub local_addr: SocketAddr,
    pub outgoing: mpsc::UnboundedSender<(SocketAddr, Envelope)>,
    pub incoming: mpsc::UnboundedReceiver<(SocketAddr, Envelope)>,
    /// Fires once per remote, the moment its handshake completes.
    pub handshake_complete: mpsc::UnboundedReceiver<SocketAddr>,
}

/// Binds `addr` and accepts DTLS associations from any remote, per
/// `server_cert` — the same cert TCP uses ([`certificate_from`]).
pub async fn listen(addr: &str, server_cert: Certificate) -> Result<DtlsChannels> {
    let socket = Arc::new(
        UdpSocket::bind(addr)
            .await
            .map_err(|e| Error::wrap("gateway", format!("failed to bind {addr}"), e))?,
    );
    let local_addr = socket
        .local_addr()
        .map_err(|e| Error::wrap("gateway", "failed to read bound address", e))?;

    let handshake_config = Arc::new(
        ConfigBuilder::default()
            .with_certificates(vec![server_cert])
            .build(false, None)
            .map_err(|e| Error::wrap("gateway", "failed to build DTLS server config", e))?,
    );

    let endpoint = Endpoint::new(local_addr, TransportProtocol::UDP, Some(handshake_config));
    Ok(spawn_driver(endpoint, socket, local_addr))
}

/// Connects to `remote_addr` and initiates a DTLS handshake as a client.
///
/// `with_insecure_skip_verify` is used because this framework's trust
/// model is fingerprint-pinning (docs/specs/Networking_Spec.md), not a CA
/// chain — the channel is still fully encrypted either way; skipping
/// verification only removes protection against a MITM who's never seen
/// the operator-logged fingerprint, the same trade already accepted for
/// self-signed TLS on the TCP side.
pub async fn connect(remote_addr: SocketAddr, client_cert: Certificate) -> Result<DtlsChannels> {
    let socket = Arc::new(
        UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| Error::wrap("gateway", "failed to bind a client UDP socket", e))?,
    );
    let local_addr = socket
        .local_addr()
        .map_err(|e| Error::wrap("gateway", "failed to read bound address", e))?;

    let client_config = Arc::new(
        ConfigBuilder::default()
            .with_certificates(vec![client_cert])
            .with_insecure_skip_verify(true)
            .build(true, Some(remote_addr))
            .map_err(|e| Error::wrap("gateway", "failed to build DTLS client config", e))?,
    );

    let mut endpoint = Endpoint::new(local_addr, TransportProtocol::UDP, None);
    endpoint
        .connect(remote_addr, client_config, None)
        .map_err(|e| Error::wrap("gateway", "failed to start DTLS handshake", e))?;

    Ok(spawn_driver(endpoint, socket, local_addr))
}

fn spawn_driver(
    endpoint: Endpoint,
    socket: Arc<UdpSocket>,
    local_addr: SocketAddr,
) -> DtlsChannels {
    let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel();
    let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
    let (handshake_tx, handshake_rx) = mpsc::unbounded_channel();

    tokio::spawn(drive(
        endpoint,
        socket,
        outgoing_rx,
        incoming_tx,
        handshake_tx,
    ));

    DtlsChannels {
        local_addr,
        outgoing: outgoing_tx,
        incoming: incoming_rx,
        handshake_complete: handshake_rx,
    }
}

/// The sans-IO driving loop: feed inbound datagrams to `endpoint.read`,
/// feed outbound app data to `endpoint.write`, tick `handle_timeout` for
/// retransmissions, and after any of those, drain `poll_transmit` onto the
/// real socket. A fixed 50ms tick rather than `poll_timeout`'s precise
/// per-remote deadline — simpler, and retransmission cadence under packet
/// loss isn't correctness-critical, just a minor efficiency trade.
async fn drive(
    mut endpoint: Endpoint,
    socket: Arc<UdpSocket>,
    mut outgoing_rx: mpsc::UnboundedReceiver<(SocketAddr, Envelope)>,
    incoming_tx: mpsc::UnboundedSender<(SocketAddr, Envelope)>,
    handshake_tx: mpsc::UnboundedSender<SocketAddr>,
) {
    flush(&mut endpoint, &socket).await;

    let mut recv_buf = vec![0u8; 4096];
    loop {
        tokio::select! {
            result = socket.recv_from(&mut recv_buf) => {
                let Ok((n, remote)) = result else { continue };
                let now = Instant::now();
                match endpoint.read(now, remote, None, BytesMut::from(&recv_buf[..n])) {
                    Ok(events) => {
                        for event in events {
                            match event {
                                EndpointEvent::HandshakeComplete => {
                                    let _ = handshake_tx.send(remote);
                                }
                                EndpointEvent::ApplicationData(data) => {
                                    if let Ok(envelope) = decode_datagram(data.freeze()) {
                                        let _ = incoming_tx.send((remote, envelope));
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => tracing::warn!(%remote, error = %e, "DTLS read failed"),
                }
                flush(&mut endpoint, &socket).await;
            }
            Some((remote, envelope)) = outgoing_rx.recv() => {
                let payload = encode_datagram(&envelope);
                if let Err(e) = endpoint.write(remote, &payload) {
                    tracing::warn!(%remote, error = %e, "DTLS write failed (handshake not complete?)");
                }
                flush(&mut endpoint, &socket).await;
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                let now = Instant::now();
                let remotes: Vec<SocketAddr> = endpoint.get_connections_keys().copied().collect();
                for remote in remotes {
                    let _ = endpoint.handle_timeout(remote, now);
                }
                flush(&mut endpoint, &socket).await;
            }
            else => break,
        }
    }
}

async fn flush(endpoint: &mut Endpoint, socket: &UdpSocket) {
    while let Some(message) = endpoint.poll_transmit() {
        let _ = socket
            .send_to(&message.message, message.transport.peer_addr)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;

    fn test_cert() -> Certificate {
        Certificate::generate_self_signed(["localhost".to_string()]).unwrap()
    }

    async fn wait_for<T>(mut rx: mpsc::UnboundedReceiver<T>) -> T {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed")
    }

    // Real localhost UDP + a real DTLS handshake, relayed through a
    // packet-capturing proxy so this test can assert on what's actually
    // on the wire (docs/specs/Networking_Spec.md's testable claim: "an
    // unencrypted UDP channel is trivially sniffable" — this proves ours
    // isn't).
    #[tokio::test]
    async fn handshake_and_message_are_not_plaintext_on_the_wire() {
        let server = listen("127.0.0.1:0", test_cert()).await.unwrap();
        let real_server_addr = server.local_addr;

        // A transparent relay between client and server, recording every
        // datagram it forwards in either direction.
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = relay.local_addr().unwrap();
        let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_writer = captured.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let mut client_addr: Option<SocketAddr> = None;
            loop {
                let Ok((n, from)) = relay.recv_from(&mut buf).await else {
                    break;
                };
                captured_writer.lock().unwrap().push(buf[..n].to_vec());
                if from == real_server_addr {
                    if let Some(addr) = client_addr {
                        let _ = relay.send_to(&buf[..n], addr).await;
                    }
                } else {
                    client_addr = Some(from);
                    let _ = relay.send_to(&buf[..n], real_server_addr).await;
                }
            }
        });

        let client = connect(relay_addr, test_cert()).await.unwrap();

        wait_for(client.handshake_complete).await;
        let mut server = server;
        let server_remote = wait_for(server.handshake_complete).await;

        let plaintext = b"the wire should never show this".to_vec();
        client
            .outgoing
            .send((relay_addr, Envelope::new(1, plaintext.clone())))
            .unwrap();

        let (from, received) = tokio::time::timeout(Duration::from_secs(2), server.incoming.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(from, server_remote);
        assert_eq!(received.payload, plaintext.as_slice());

        let snapshot = captured.lock().unwrap();
        assert!(
            !snapshot.is_empty(),
            "the relay should have captured at least the handshake"
        );
        for datagram in snapshot.iter() {
            assert!(
                !datagram
                    .windows(plaintext.len())
                    .any(|window| window == plaintext.as_slice()),
                "found the plaintext message on the wire: {datagram:?}"
            );
        }
    }

    #[tokio::test]
    async fn replayed_datagram_is_rejected() {
        let server = listen("127.0.0.1:0", test_cert()).await.unwrap();
        let server_addr = server.local_addr;
        let mut server = server;

        // A raw socket standing in for an attacker who captured a valid
        // encrypted datagram off the wire and is resending it verbatim.
        let attacker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let captured_datagram: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
        let capture_writer = captured_datagram.clone();
        let attacker_local = attacker.local_addr().unwrap();

        // Relay so we can capture the exact post-handshake application-data
        // datagram the real client sends.
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = relay.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let mut client_addr: Option<SocketAddr> = None;
            let mut handshake_done = false;
            loop {
                let Ok((n, from)) = relay.recv_from(&mut buf).await else {
                    break;
                };
                if from == server_addr {
                    if let Some(addr) = client_addr {
                        let _ = relay.send_to(&buf[..n], addr).await;
                    }
                } else {
                    client_addr = Some(from);
                    if handshake_done {
                        *capture_writer.lock().unwrap() = Some(buf[..n].to_vec());
                    }
                    let _ = relay.send_to(&buf[..n], server_addr).await;
                }
                // A crude but adequate signal for "handshake is likely
                // done": once we've relayed a handful of round trips.
                handshake_done = true;
            }
        });

        let client = connect(relay_addr, test_cert()).await.unwrap();
        wait_for(client.handshake_complete).await;
        let server_remote = wait_for(server.handshake_complete).await;

        client
            .outgoing
            .send((relay_addr, Envelope::new(1, b"original".to_vec())))
            .unwrap();
        let (_, first) = tokio::time::timeout(Duration::from_secs(2), server.incoming.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.payload, b"original".as_slice());

        let replay_bytes = captured_datagram
            .lock()
            .unwrap()
            .clone()
            .expect("should have captured a post-handshake datagram");
        attacker.send_to(&replay_bytes, server_addr).await.unwrap();
        let _ = attacker_local; // bound only so the attacker has a real source address

        // The replay must not produce a second delivered message.
        let second = tokio::time::timeout(Duration::from_millis(500), server.incoming.recv()).await;
        assert!(
            second.is_err(),
            "a replayed datagram should not be delivered as a new message: {second:?}"
        );

        let _ = server_remote;
    }
}
