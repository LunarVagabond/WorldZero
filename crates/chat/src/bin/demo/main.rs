//! `cargo run -p chat --bin demo -- <username> --password <pw> [--register] [--no-gateway] [--gateway-addr <host:port>]`
//! — an interactive two-terminal chat client. By default it connects
//! through the real `gateway` TCP+TLS transport to `bin/gateway_server`
//! (start that first — `make chat-server`, or `cargo run -p chat --bin
//! gateway_server`) and authenticates for real via `auth`'s
//! username/password provider — `--password` is required in this mode,
//! and `--register` creates the account on first use instead of logging
//! into an existing one. `--no-gateway` talks straight to Postgres/Redis
//! instead, bypassing the netcode (and auth) entirely — useful when
//! iterating on chat's own logic without a gateway server running; no
//! password needed there, it uses the same stable demo-account shortcut
//! as before.
//!
//! Run in two terminals with different usernames. Both auto-join a shared
//! "demo" channel; from there:
//!   /join <name>    join (creating if needed) another channel, and switch to it
//!   /leave <name>   leave a channel
//!   /switch <name>  switch which joined channel plain text sends to
//!   /who            list joined channels and which is current
//!   /help           show this list
//! Anything else is sent as a chat message to the current channel.
//!
//! Needs WZ_POSTGRES_*/WZ_REDIS_* set (`.env`) either way — gateway mode
//! needs them on the server side, direct mode needs them here too.

mod commands;
mod direct_client;
mod gateway_client;

const DEFAULT_GATEWAY_ADDR: &str = "127.0.0.1:7800";

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let mut username = None;
    let mut password = None;
    let mut register = false;
    let mut use_gateway = true;
    let mut gateway_addr =
        std::env::var("WZ_CHAT_GATEWAY_ADDR").unwrap_or_else(|_| DEFAULT_GATEWAY_ADDR.to_string());

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--no-gateway" => use_gateway = false,
            "--register" => register = true,
            "--password" => match args.next() {
                Some(value) => password = Some(value),
                None => {
                    eprintln!("--password needs a value");
                    std::process::exit(2);
                }
            },
            "--gateway-addr" => match args.next() {
                Some(value) => gateway_addr = value,
                None => {
                    eprintln!("--gateway-addr needs a value");
                    std::process::exit(2);
                }
            },
            other if username.is_none() => username = Some(other.to_string()),
            other => {
                eprintln!("unrecognized argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let Some(username) = username else {
        eprintln!(
            "usage: demo <username> --password <pw> [--register] [--no-gateway] [--gateway-addr <host:port>]"
        );
        std::process::exit(2);
    };

    let result = if use_gateway {
        let Some(password) = password else {
            eprintln!(
                "gateway mode needs --password <pw> (add --register when creating a new account)"
            );
            std::process::exit(2);
        };
        gateway_client::run(&username, &password, register, &gateway_addr).await
    } else {
        direct_client::run(&username).await
    };

    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
