use std::{io, path::PathBuf};

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("invalid {kind} '{value}': {reason}")]
    InvalidName {
        kind: &'static str,
        value: String,
        reason: &'static str,
    },

    #[error("invalid memory id '{0}': expected '<repo|agent>/<owner>/[<folder>/...]<slug>'")]
    InvalidId(String),

    #[error("memory '{0}' not found")]
    NotFound(String),

    #[error("memory '{0}' already exists")]
    AlreadyExists(String),

    #[error(
        "version conflict on '{id}': expected version {expected}, current version is {current}"
    )]
    VersionConflict {
        id: String,
        expected: u64,
        current: u64,
    },

    #[error("malformed memory file {}: {reason}", path.display())]
    Malformed { path: PathBuf, reason: String },

    #[error("invalid input: {0}")]
    Invalid(String),

    #[error("io error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub(crate) trait IoContext<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|source| StoreError::Io {
            path: path.into(),
            source,
        })
    }
}
