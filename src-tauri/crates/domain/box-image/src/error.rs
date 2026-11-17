//! Error type for the dshimage parser and writer. Every variant carries a
//! line number when it makes sense, so the CLI / UI can point at the exact
//! offending token in a user-authored script.

use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub enum ImageError {
    Syntax { line: usize, message: String },
    MissingDirective { line: usize, name: &'static str },
    InvalidSource { line: usize, source: String, reason: String },
    UnsafePath { line: usize, path: String },
    InvalidManifest(String),
    ArchiveMissingManifest(PathBuf),
    Io(std::io::Error),
    Serde(serde_json::Error),
}

impl fmt::Display for ImageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Syntax { line, message } => {
                write!(formatter, "line {line}: {message}")
            }
            ImageError::MissingDirective { line, name } => {
                write!(formatter, "line {line}: missing required directive {name}")
            }
            ImageError::InvalidSource { line, source, reason } => {
                write!(formatter, "line {line}: invalid source `{source}` ({reason})")
            }
            ImageError::UnsafePath { line, path } => {
                write!(formatter, "line {line}: unsafe path `{path}`")
            }
            ImageError::InvalidManifest(message) => {
                write!(formatter, "manifest is not a valid dshimage: {message}")
            }
            ImageError::ArchiveMissingManifest(path) => {
                write!(formatter, "archive `{}` is missing `manifest.json`", path.display())
            }
            ImageError::Io(error) => write!(formatter, "io error: {error}"),
            ImageError::Serde(error) => write!(formatter, "serde error: {error}"),
        }
    }
}

impl std::error::Error for ImageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ImageError::Io(error) => Some(error),
            ImageError::Serde(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ImageError {
    fn from(error: std::io::Error) -> Self {
        ImageError::Io(error)
    }
}

impl From<serde_json::Error> for ImageError {
    fn from(error: serde_json::Error) -> Self {
        ImageError::Serde(error)
    }
}

impl From<String> for ImageError {
    fn from(message: String) -> Self {
        ImageError::Syntax { line: 0, message }
    }
}
