use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("config error: {0}")]
    Config(String),

    #[error("{0}")]
    Invalid(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("ambiguous: {0}")]
    Ambiguous(String),

    /// Archive failed validation and must not be installed.
    #[error("unsafe archive rejected: {0}")]
    UnsafeArchive(String),

    #[error("archive error: {0}")]
    Archive(String),

    /// An operation is refused because it would be unsafe to proceed
    /// (e.g. interrupted deployment pending, externally modified files).
    #[error("blocked: {0}")]
    Blocked(String),

    /// A deployment operation failed (message includes rollback status).
    #[error("{0}")]
    Deploy(String),

    /// Malformed or self-contradictory FOMOD installer data.
    #[error("fomod: {0}")]
    Fomod(String),

    /// A FOMOD feature lmm does not implement, where proceeding would
    /// change which files get installed.
    #[error("unsupported FOMOD feature: {0}")]
    FomodUnsupported(String),

    /// The user cancelled an interactive flow. Not a fault: frontends
    /// report it calmly and exit cleanly.
    #[error("cancelled")]
    Cancelled,
}

impl Error {
    /// Wrap an io::Error with the path it concerns; bare io::Errors
    /// ("permission denied" with no context) make terrible messages.
    pub fn io(path: impl AsRef<Path>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.as_ref().to_path_buf(),
            source,
        }
    }
}

/// Extension to attach path context to io results in one call.
pub trait IoContext<T> {
    fn path_ctx(self, path: impl AsRef<Path>) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn path_ctx(self, path: impl AsRef<Path>) -> Result<T> {
        self.map_err(|e| Error::io(path, e))
    }
}
