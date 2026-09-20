//! Stable rendezvous for stores whose paths exceed Unix socket address limits;
//! the resolution itself lives in the shared client crate.
use agent_runtime::{Error, Result};
use std::path::{Path, PathBuf};

pub fn default_socket(store: &Path) -> Result<PathBuf> {
    agent_client::socket::default_socket(store).map_err(|error| match error.detail {
        Some(detail) => Error::with(&error.code, detail),
        None => Error::new(&error.code),
    })
}
