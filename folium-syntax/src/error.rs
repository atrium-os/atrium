use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// Reading the file failed.
    Io(String),
    /// The bytes at `offset` aren't PDF syntax.
    Syntax { offset: u64, what: &'static str },
    /// The syntax ran past the bytes read; the caller may read more.
    Truncated,
    /// A budget of §6 was reached.
    Limit(&'static str),
    /// The file is encrypted and neither the given password nor the empty
    /// one opens it.
    NeedsPassword,
    /// A feature this layer doesn't read.
    Unsupported(&'static str),
    /// No usable cross-reference and no objects found by the repair scan.
    Unreadable(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "reading the file: {e}"),
            Error::Syntax { offset, what } => write!(f, "at byte {offset}: {what}"),
            Error::Truncated => f.write_str("the object runs past what was read"),
            Error::Limit(what) => write!(f, "over the limit: {what}"),
            Error::NeedsPassword => f.write_str("needs a password"),
            Error::Unsupported(what) => write!(f, "not supported: {what}"),
            Error::Unreadable(what) => write!(f, "unreadable: {what}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self { Error::Io(e.to_string()) }
}

pub type Result<T> = std::result::Result<T, Error>;
