//! Typed errors at the command level and their exit classes.

use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Usage(String),
    #[error("config {}: {message}", file.display())]
    Config { file: PathBuf, message: String },
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    NotATerminal(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Process exit codes.
pub mod exit {
    pub const OK: u8 = 0;
    pub const ERROR: u8 = 1;
    /// Some work was done, some was not: rows with errors, removals blocked.
    pub const PARTIAL: u8 = 3;
    pub const USAGE: u8 = 64;
    /// The picker was dismissed.
    pub const CANCELLED: u8 = 130;
}

impl Error {
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Usage(_) | Error::Config { .. } => exit::USAGE,
            Error::Io { .. } | Error::NotATerminal(_) | Error::Other(_) => exit::ERROR,
        }
    }
}
