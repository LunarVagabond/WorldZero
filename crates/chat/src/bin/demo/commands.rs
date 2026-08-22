//! Slash-command parsing for the interactive demo client, shared between
//! its gateway and direct transport modes.

pub enum Command<'a> {
    Send(&'a str),
    Join(&'a str),
    Leave(&'a str),
    Switch(&'a str),
    Who,
    Help,
    Unknown(&'a str),
}

/// `None` for a blank line — nothing to do, not even re-print the prompt
/// differently.
pub fn parse(line: &str) -> Option<Command<'_>> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    Some(if let Some(rest) = line.strip_prefix("/join ") {
        Command::Join(rest.trim())
    } else if let Some(rest) = line.strip_prefix("/leave ") {
        Command::Leave(rest.trim())
    } else if let Some(rest) = line.strip_prefix("/switch ") {
        Command::Switch(rest.trim())
    } else if line == "/who" {
        Command::Who
    } else if line == "/help" {
        Command::Help
    } else if let Some(name) = line.strip_prefix('/') {
        Command::Unknown(name)
    } else {
        Command::Send(line)
    })
}

pub const HELP_TEXT: &str = "\
/join <name>    join (creating if needed) a channel and switch to it
/leave <name>   leave a channel
/switch <name>  switch which joined channel plain text sends to
/who            list joined channels and which is current
/help           show this list";
