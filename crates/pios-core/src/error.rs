use std::fmt;
use std::io;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io { context: String, source: io::Error },
    Config(String),
    Other(String),
}

impl Error {
    pub fn msg(m: impl Into<String>) -> Self {
        Error::Other(m.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { context, source } => write!(f, "{context}: {source}"),
            Error::Config(m) => write!(f, "config: {m}"),
            Error::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(source: io::Error) -> Self {
        Error::Io { context: "io".into(), source }
    }
}

/// Attach a human readable context to io errors.
pub trait IoContext<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|source| Error::Io { context: context(), source })
    }
}
