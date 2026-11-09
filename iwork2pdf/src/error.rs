//! Library error type.

use std::path::PathBuf;

/// Everything that can stop a document from converting.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file or directory could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file involved.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// A document (or its nested Index.zip) is not a readable zip archive.
    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),
    /// The package holds no IWA archives (an iWork '09 or older document).
    #[error("no Index/*.iwa archives: not an iWork 2013+ document")]
    NotIwa,
    /// A required archive object is absent.
    #[error("missing {0}")]
    Missing(&'static str),
    /// The file extension is not .numbers, .key or .pages.
    #[error("unsupported document type '{0}'")]
    Unsupported(String),
    /// Writing the PDF failed.
    #[error("pdf: {0}")]
    Pdf(String),
}

/// Result alias for library functions.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Wraps an I/O error with the path it concerns.
    ///
    /// # Arguments
    ///
    /// * `path` - The file or directory the operation was on
    ///
    /// # Returns
    ///
    /// A closure for `map_err`, e.g. `fs::read(p).map_err(Error::io(p))`.
    pub fn io(path: &std::path::Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
        move |source| Error::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}
