//! Native file tools operate only on regular files. Special files need an I/O
//! owner with cancellation semantics, not Tokio's blocking filesystem workers.
use super::FILE_BYTES;
use crate::{
    Error, Result, fail,
    sandbox::{Policy, WRITE},
};
use serde_json::Value;
use std::{io::ErrorKind, path::Path, process::Stdio, sync::Arc};
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

/// The first `count` bytes of a file, or fewer when it is shorter.
pub(super) async fn read_head(path: &Path, count: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (open_regular(path, false).await?)
        .take(count)
        .read_to_end(&mut bytes)
        .await?;
    Ok(bytes)
}

/// Write `content` to `path`. A confined write runs inside the bot's
/// sandbox, in the agent binary, as Codex runs `apply_patch`: the kernel
/// decides what it reaches, so a link swapped in after the path was
/// checked can't carry it out. An unconfined one makes the folders above
/// `path` itself first.
pub(super) async fn write(path: &Path, content: &[u8], sandbox: Option<Arc<Policy>>) -> Result<()> {
    if let Some(policy) = sandbox {
        return confined(&policy, path, content).await;
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = open_regular(path, true).await?;
    file.set_len(0).await?;
    file.write_all(content).await?;
    // Tokio buffers writes in its blocking pool. Await their completion before
    // reporting success and recording the tool's durable result.
    file.flush().await?;
    Ok(())
}

async fn confined(policy: &Policy, path: &Path, content: &[u8]) -> Result<()> {
    let Some(helper) = policy.helper.as_deref().and_then(Path::to_str) else {
        return fail("sandbox_unavailable");
    };
    let Some(target) = path.to_str() else {
        return fail("path_not_utf8");
    };
    let _held = crate::sandbox::hold(policy).await;
    let mut command = crate::sandbox::command(policy, helper, &[WRITE, target], false)?;
    command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().ok_or(Error::new("io_error"))?;
    // A helper that fails before reading closes stdin; its own error says why.
    let _ = stdin.write_all(content).await;
    drop(stdin);
    let output = child.wait_with_output().await?;
    if output.status.success() {
        return Ok(());
    }
    // The helper reports as the CLI does: one JSON object on stderr.
    let reported: Option<Value> = serde_json::from_slice(&output.stderr).ok();
    let code = reported.as_ref().and_then(|r| r["error"].as_str());
    let detail = reported.as_ref().and_then(|r| r["detail"].as_str());
    Err(match (code, detail) {
        (Some(code), Some(detail)) => Error::with(code, detail),
        (Some(code), None) => Error::new(code),
        _ => Error::with(
            "io_error",
            format!("the confined write exited with {}", output.status),
        ),
    })
}
