//! Shared error/result types used across crate boundaries.

use std::error::Error as StdError;
use std::fmt;

#[derive(Debug)]
pub struct Error {
    source_crate: &'static str,
    message: String,
    source: Option<Box<dyn StdError + Send + Sync + 'static>>,
}

impl Error {
    pub fn new(source_crate: &'static str, message: impl Into<String>) -> Self {
        Self {
            source_crate,
            message: message.into(),
            source: None,
        }
    }

    pub fn wrap(
        source_crate: &'static str,
        message: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            source_crate,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.source_crate, self.message)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_ref()
            .map(|e| e.as_ref() as &(dyn StdError + 'static))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// `?`-friendly conversion from any `std::error::Error` into [`Error`].
pub trait ResultExt<T> {
    fn context(self, source_crate: &'static str, message: impl Into<String>) -> Result<T>;
}

impl<T, E> ResultExt<T> for std::result::Result<T, E>
where
    E: StdError + Send + Sync + 'static,
{
    fn context(self, source_crate: &'static str, message: impl Into<String>) -> Result<T> {
        self.map_err(|e| Error::wrap(source_crate, message, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_includes_crate_and_message() {
        let err = Error::new("auth", "token expired");
        assert_eq!(err.to_string(), "[auth] token expired");
    }

    #[test]
    fn wrap_preserves_source_chain() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing file");
        let err = Error::wrap("content", "failed to load manifest", io_err);

        assert_eq!(err.to_string(), "[content] failed to load manifest");
        assert!(StdError::source(&err).is_some());
    }

    #[test]
    fn context_converts_via_question_mark() {
        fn parse(input: &str) -> Result<u16> {
            input.parse::<u16>().context("common", "bad port")
        }

        let err = parse("not-a-number").unwrap_err();
        assert_eq!(err.to_string(), "[common] bad port");
    }
}
