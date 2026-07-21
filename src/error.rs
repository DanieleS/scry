//! Error type for the engine. Deliberately small — the crate has few failure
//! modes, and each one carries just enough to diagnose a broken profile or a
//! process that has moved out from under us.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// The underlying OS read failed (bad address, process gone, no permission).
    Io(std::io::Error),
    /// The OS read fewer bytes than requested — a partial read we refuse to
    /// treat as success, so a half-resolved pointer never surfaces as a value.
    ShortRead { expected: usize, got: usize },
    /// A module named in a profile was not mapped in the target process.
    ModuleNotFound(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "memory read failed: {e}"),
            Error::ShortRead { expected, got } => {
                write!(f, "short read: wanted {expected} bytes, got {got}")
            }
            Error::ModuleNotFound(name) => write!(f, "module not mapped: {name}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
