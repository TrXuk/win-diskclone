//! Error types for diskclone.

use std::io;

use thiserror::Error;

/// Result type for diskclone operations.
pub type Result<T> = std::result::Result<T, DiskCloneError>;

/// Errors that can occur during disk cloning.
#[derive(Error, Debug)]
pub enum DiskCloneError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("VSS error: {0}")]
    Vss(String),

    #[error("SSH error: {0}")]
    Ssh(#[from] ssh2::Error),

    #[error("{0}")]
    Other(String),
}

impl From<Box<dyn std::error::Error + Send + Sync>> for DiskCloneError {
    fn from(e: Box<dyn std::error::Error + Send + Sync>) -> Self {
        DiskCloneError::Vss(e.to_string())
    }
}
