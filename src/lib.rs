pub mod history;
pub mod output;
pub mod provider;
pub mod sse;
pub mod store;
pub mod tools;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub struct Error(pub String);
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self("io_error".into())
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self("invalid_json".into())
    }
}
pub fn fail<T>(message: &str) -> Result<T> {
    Err(Error(message.into()))
}
