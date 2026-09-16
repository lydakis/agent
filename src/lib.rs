pub mod codec;
pub mod output;
pub mod provider;
pub mod sse;
pub mod store;
pub mod tools;

pub type Result<T> = std::result::Result<T, Error>;

/// A stable machine code plus optional human detail. Codes never contain
/// URLs, credentials, or prompt text; detail may carry a provider message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub code: String,
    pub detail: Option<String>,
}
impl Error {
    pub fn new(code: &str) -> Self {
        Self {
            code: code.into(),
            detail: None,
        }
    }
    pub fn with(code: &str, detail: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            detail: Some(detail.into()),
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.detail {
            Some(detail) => write!(f, "{}: {detail}", self.code),
            None => f.write_str(&self.code),
        }
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self::new("io_error")
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::new("invalid_json")
    }
}
pub fn fail<T>(code: &str) -> Result<T> {
    Err(Error::new(code))
}
pub fn fail_with<T>(code: &str, detail: impl Into<String>) -> Result<T> {
    Err(Error::with(code, detail))
}
