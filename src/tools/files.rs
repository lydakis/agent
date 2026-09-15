//! Native file tools operate only on regular files. Special files need an I/O
//! owner with cancellation semantics, not Tokio's blocking filesystem workers.
use super::FILE_BYTES;
use crate::{Error, Result, fail};
use std::{io::ErrorKind, path::Path};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn open_regular(path: &Path, write: bool) -> Result<tokio::fs::File> {
    let open_error = |_| {
        if write {
            Error::new("io_error")
        } else {
            Error::with("file_unreadable", path.display().to_string())
        }
    };
    // Avoid opening known devices/pipes at all. Follow symlinks as normal file
    // tools do, then verify the descriptor too in case the path changes.
    match tokio::fs::metadata(path).await {
        Ok(metadata) if !metadata.is_file() => return fail("file_not_regular"),
        Ok(_) => {}
        Err(error) if write && error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(open_error(error)),
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.read(!write).write(write).create(write);
    // A FIFO substituted after metadata must not block even during open.
    // O_NONBLOCK does not change regular-file I/O. Never truncate before fstat.
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY);
    let file = options.open(path).await.map_err(open_error)?;
    if !file.metadata().await?.is_file() {
        return fail("file_not_regular");
    }
    Ok(file)
}

pub(super) async fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let file = open_regular(path, false).await?;
    let mut bytes = Vec::new();
    file.take(FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > FILE_BYTES {
        return fail("file_too_large");
    }
    Ok(bytes)
}

pub(super) async fn write(path: &Path, content: &[u8]) -> Result<()> {
    let mut file = open_regular(path, true).await?;
    file.set_len(0).await?;
    file.write_all(content).await?;
    // Tokio buffers writes in its blocking pool. Await their completion before
    // reporting success and recording the tool's durable result.
    file.flush().await?;
    Ok(())
}
