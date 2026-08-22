//! Gateway-mode transport for the demo client: connects to
//! `bin/gateway_server` over the real TCP+TLS `gateway` transport instead
//! of touching Postgres/Redis directly. The default mode — `--no-gateway`
//! switches to `direct_client` instead.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use chat::gateway_protocol::{ClientMessage, ServerMessage};
use common::id::ChannelId;
use common::{Error, Result};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::io::AsyncBufReadExt;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::RootCertStore;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};

use super::commands::{self, Command};

/// The TLS `ServerName` a demo connection presents — matches the "localhost"
/// SAN `gateway::tls`'s self-signed certs are always generated with.
const SERVER_NAME: &str = "localhost";
const DEFAULT_CHANNEL: &str = "demo";

pub async fn run(username: &str, password: &str, register: bool, addr: &str) -> Result<()> {
    gateway::tcp::ensure_crypto_provider_installed();

    // Demo convenience: client and server share the same config dir
    // (`WZ_CONFIG_DIR`/`./config`), so whichever process starts first
    // mints the self-signed cert and the other just reads it back —
    // that's how the client ends up trusting the right one without any
    // out-of-band fingerprint exchange. Start `chat-server` first in
    // practice, or a race here can leave the two sides with different
    // certs (surfaces as a TLS handshake failure below).
    let config_dir = common::config::config_dir();
    let cert = gateway::tls::load_or_generate(&config_dir)?;

    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(cert.cert_der.clone()))
        .map_err(|e| Error::wrap("chat", "failed to trust the gateway's certificate", e))?;
    let client_config = tokio_rustls::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));

    let tcp = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
        Error::wrap(
            "chat",
            format!("failed to connect to the gateway at {addr} — is `make chat-server` running?"),
            e,
        )
    })?;
    let server_name = ServerName::try_from(SERVER_NAME)
        .map_err(|e| Error::wrap("chat", "invalid TLS server name", e))?;
    let tls = connector.connect(server_name, tcp).await.map_err(|e| {
        Error::wrap(
            "chat",
            "TLS handshake with the gateway failed (mismatched cert? restart chat-server and demo together)",
            e,
        )
    })?;

    let framed = tokio_util::codec::Framed::new(tls, gateway::EnvelopeCodec::default());
    let (mut sink, mut stream) = framed.split();

    let auth_request = if register {
        auth::gateway_protocol::ClientMessage::Register {
            username: username.to_string(),
            password: password.to_string(),
        }
    } else {
        auth::gateway_protocol::ClientMessage::Login {
            username: username.to_string(),
            password: password.to_string(),
        }
    };
    send_auth(&mut sink, &auth_request).await?;
    match recv_auth(&mut stream).await? {
        auth::gateway_protocol::ServerMessage::Authenticated { .. } => {}
        auth::gateway_protocol::ServerMessage::Error { message } => {
            return Err(Error::new(
                "chat",
                format!("authentication failed: {message}"),
            ));
        }
    }

    let mut joined: HashMap<String, ChannelId> = HashMap::new();

    // Explicitly join and wait for the confirmation, rather than relying
    // on an implicit server-side auto-join — that raced against stdin: a
    // buffered first line could get processed as a `Send` before the
    // un-awaited `Joined` for the default channel had arrived.
    send(
        &mut sink,
        &ClientMessage::Join {
            channel: DEFAULT_CHANNEL.to_string(),
        },
    )
    .await?;
    let mut current = match recv(&mut stream).await? {
        ServerMessage::Joined {
            channel_id,
            channel,
        } => {
            joined.insert(channel.clone(), channel_id);
            Some(channel)
        }
        other => {
            return Err(Error::new(
                "chat",
                format!("expected Joined, got {other:?}"),
            ));
        }
    };

    println!("Connected to the chat gateway at {addr} as {username}.");
    println!("joined #{DEFAULT_CHANNEL} (now current)");
    println!("{}", commands::HELP_TEXT);

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    prompt();

    loop {
        tokio::select! {
            frame = stream.next() => {
                let Some(frame) = frame else {
                    println!("gateway closed the connection");
                    break;
                };
                let envelope = frame.map_err(|e| Error::wrap("chat", "connection error", e))?;
                match ServerMessage::from_envelope(&envelope)? {
                    ServerMessage::Joined { channel_id, channel } => {
                        joined.insert(channel.clone(), channel_id);
                        current = Some(channel.clone());
                        println!("joined #{channel} (now current)");
                    }
                    ServerMessage::Left { channel } => {
                        joined.remove(&channel);
                        if current.as_deref() == Some(channel.as_str()) {
                            current = joined.keys().next().cloned();
                        }
                        println!("left #{channel}");
                    }
                    ServerMessage::Chat { channel, sender, body, .. } => {
                        println!("[{channel}] {sender}: {body}");
                    }
                    ServerMessage::Error { message } => {
                        println!("error: {message}");
                    }
                }
                prompt();
            }
            line = lines.next_line() => {
                let Some(line) = line.map_err(|e| Error::wrap("chat", "failed to read stdin", e))? else {
                    break;
                };
                let Some(command) = commands::parse(&line) else {
                    prompt();
                    continue;
                };

                match command {
                    Command::Send(body) => {
                        match current.as_ref().and_then(|c| joined.get(c).copied()) {
                            Some(channel_id) => {
                                send(&mut sink, &ClientMessage::Send { channel_id, body: body.to_string() }).await?;
                            }
                            None => println!("not in a channel yet — /join <name> first"),
                        }
                    }
                    Command::Join(name) => {
                        if name.is_empty() {
                            println!("usage: /join <name>");
                        } else {
                            send(&mut sink, &ClientMessage::Join { channel: name.to_string() }).await?;
                        }
                    }
                    Command::Leave(name) => {
                        if name.is_empty() {
                            println!("usage: /leave <name>");
                        } else {
                            send(&mut sink, &ClientMessage::Leave { channel: name.to_string() }).await?;
                        }
                    }
                    Command::Switch(name) => {
                        if name.is_empty() {
                            println!("usage: /switch <name>");
                        } else if joined.contains_key(name) {
                            current = Some(name.to_string());
                            println!("now sending to #{name}");
                        } else {
                            println!("not joined to #{name} — /join it first");
                        }
                    }
                    Command::Who => {
                        for name in joined.keys() {
                            let marker = if current.as_deref() == Some(name.as_str()) { "*" } else { " " };
                            println!("{marker} {name}");
                        }
                    }
                    Command::Help => println!("{}", commands::HELP_TEXT),
                    Command::Unknown(cmd) => println!("unknown command: /{cmd} — try /help"),
                }
                prompt();
            }
        }
    }

    Ok(())
}

fn prompt() {
    print!("> ");
    std::io::stdout().flush().ok();
}

async fn send(
    sink: &mut (impl Sink<gateway::Envelope, Error = std::io::Error> + Unpin),
    message: &ClientMessage,
) -> Result<()> {
    let envelope = message.into_envelope()?;
    sink.send(envelope)
        .await
        .map_err(|e| Error::wrap("chat", "failed to send to the gateway", e))
}

async fn recv(
    stream: &mut (impl Stream<Item = std::result::Result<gateway::Envelope, std::io::Error>> + Unpin),
) -> Result<ServerMessage> {
    let frame = stream
        .next()
        .await
        .ok_or_else(|| Error::new("chat", "gateway closed the connection before Joined"))?;
    let envelope = frame.map_err(|e| Error::wrap("chat", "connection error", e))?;
    ServerMessage::from_envelope(&envelope)
}

async fn send_auth(
    sink: &mut (impl Sink<gateway::Envelope, Error = std::io::Error> + Unpin),
    message: &auth::gateway_protocol::ClientMessage,
) -> Result<()> {
    let envelope = message.into_envelope()?;
    sink.send(envelope)
        .await
        .map_err(|e| Error::wrap("chat", "failed to send to the gateway", e))
}

async fn recv_auth(
    stream: &mut (impl Stream<Item = std::result::Result<gateway::Envelope, std::io::Error>> + Unpin),
) -> Result<auth::gateway_protocol::ServerMessage> {
    let frame = stream.next().await.ok_or_else(|| {
        Error::new(
            "chat",
            "gateway closed the connection before authenticating",
        )
    })?;
    let envelope = frame.map_err(|e| Error::wrap("chat", "connection error", e))?;
    auth::gateway_protocol::ServerMessage::from_envelope(&envelope)
}
