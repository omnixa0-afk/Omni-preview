use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum OmniError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("storage error: {0}")]
    Storage(String),
    /// The database is encrypted and this machine cannot unwrap its key (a folder from another machine
    /// or Windows account): the recovery phrase of a profile in it opens it.
    #[error("storage locked: {0}")]
    StorageLocked(String),
    #[error("unsupported legacy local data: {0}")]
    UnsupportedLegacyLocalData(String),
    #[error("unsupported database format: {0}")]
    UnsupportedDatabaseFormat(String),
    #[error("unsupported legacy profile state: {0}")]
    UnsupportedLegacyProfileState(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type OmniResult<T> = Result<T, OmniError>;
