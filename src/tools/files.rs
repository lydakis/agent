//! Native file tools operate only on regular files. Special files need an I/O
//! owner with cancellation semantics, not Tokio's blocking filesystem workers.
use super::FILE_BYTES;
use crate::{
    Error, Result, fail,
    sandbox::{Policy, WRITE},
};
use serde_json::Value;
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Open `path`, a regular file, refused when what was opened is, or is
/// under, one of `hidden`: the paths a sandboxed bot may not read
/// ([`Policy::unreadable`]). The file opened is checked, not the path, so a
/// link swapped between a check and the open can't reach one.
async fn open_regular(path: &Path, write: bool, hidden: &[PathBuf]) -> Result<tokio::fs::File> {
    let open_error = |error: std::io::Error| {
        if write {
            Error::with("io_error", format!("{}: {error}", path.display()))
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
    let mut options = std::fs::OpenOptions::new();
    options.read(!write).write(write).create(write);
    // A FIFO substituted after metadata must not block even during open.
    // O_NONBLOCK does not change regular-file I/O. Never truncate before fstat.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    // Opened, and where it is looked up, in one job on the blocking pool,
    // as Tokio's own open would be: the lookup is filesystem work too.
    let (target, hidden) = (path.to_owned(), hidden.to_vec());
    let (file, denied) = tokio::task::spawn_blocking(move || {
        let file = options.open(&target)?;
        let denied = agent_client::hidden::holds(&hidden, &file);
        Ok::<_, std::io::Error>((file, denied))
    })
    .await
    .map_err(|_| Error::new("io_error"))?
    .map_err(open_error)?;
    if denied {
        return Err(Error::with(
            "read_denied",
            format!(
                "{} is kept unreadable in this bot's sandbox",
                path.display()
            ),
        ));
    }
    let file = tokio::fs::File::from_std(file);
    if !file.metadata().await?.is_file() {
        return fail("file_not_regular");
    }
    Ok(file)
}

/// A file's bytes, up to the tools' limit; `hidden` as [`open_regular`]
/// takes it.
pub(super) async fn read_bounded(path: &Path, hidden: &[PathBuf]) -> Result<Vec<u8>> {
    let file = open_regular(path, false, hidden).await?;
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
pub(super) async fn read_head(path: &Path, count: u64, hidden: &[PathBuf]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (open_regular(path, false, hidden).await?)
        .take(count)
        .read_to_end(&mut bytes)
        .await?;
    Ok(bytes)
}

/// Write `content` to `path`. A confined write runs inside the bot's
/// sandbox, in the agent binary, as Codex runs `apply_patch`: the kernel
/// decides what it reaches, so a link swapped in after the path was
/// checked can't carry it out. An unconfined one makes the folders above
/// `path` itself first. The caller holds the sandbox's placeholders.
pub(super) async fn write(path: &Path, content: &[u8], sandbox: Option<Arc<Policy>>) -> Result<()> {
    if let Some(policy) = sandbox {
        return confined(&policy, path, content).await;
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = open_regular(path, true, &[]).await?;
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
