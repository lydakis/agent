//! Registered tools share an execution budget; no per-bot worker or shell exists.
//! Tools run with the caller's OS permissions inside the bot workspace path;
//! nothing here is a sandbox.
use crate::{Error, Result, codec::ToolSchema, fail};
use serde::Deserialize;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::Semaphore,
};

mod files;
use files::read_bounded;

/// Model-facing text per tool result stream.
pub const PREVIEW_BYTES: usize = 64 * 1024;
/// Retained full output per stream; more than this fails the tool.
pub const ARTIFACT_BYTES: usize = 1024 * 1024;
const FILE_BYTES: usize = 4 * 1024 * 1024;
const WRITE_BYTES: usize = 1024 * 1024;
const DEFAULT_SHELL_TIMEOUT_MS: u64 = 120_000;
const MAX_SHELL_TIMEOUT_MS: u64 = 600_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
    Echo,
    Shell,
    Read,
    Write,
    Edit,
}
impl Tool {
    fn parse(name: &str) -> Option<Tool> {
        Some(match name {
            "echo" => Tool::Echo,
            "shell" => Tool::Shell,
            "read" => Tool::Read,
            "write" => Tool::Write,
            "edit" => Tool::Edit,
            _ => return None,
        })
    }
    fn name(self) -> &'static str {
        match self {
            Tool::Echo => "echo",
            Tool::Shell => "shell",
            Tool::Read => "read",
            Tool::Write => "write",
            Tool::Edit => "edit",
        }
    }
    fn schema(self) -> ToolSchema {
        let (description, parameters) = match self {
            Tool::Echo => (
                "Return the supplied text unchanged.",
                json!({"type":"object",
                "properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
            ),
            Tool::Shell => (
                "Run a noninteractive /bin/sh command in the workspace. No background jobs survive the call. stdout and stderr are returned separately; each is truncated to a head and tail beyond 64 KiB, with the full output retained. Default timeout 120 s, maximum 600 s.",
                json!({"type":"object","properties":{"command":{"type":"string"},
                "timeout_ms":{"type":"integer","minimum":1,"maximum":MAX_SHELL_TIMEOUT_MS}},
                "required":["command"],"additionalProperties":false}),
            ),
            Tool::Read => (
                "Read a UTF-8 text file with line numbers. Paths are relative to the workspace unless absolute. Use offset (1-based line) and limit (lines, default 500) to page through large files.",
                json!({"type":"object","properties":{"path":{"type":"string"},
                "offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":5000}},
                "required":["path"],"additionalProperties":false}),
            ),
            Tool::Write => (
                "Create or overwrite a file with the given content, creating parent directories. Content is limited to 1 MiB.",
                json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},
                "required":["path","content"],"additionalProperties":false}),
            ),
            Tool::Edit => (
                "Replace an exact string in a file. old must occur exactly once unless replace_all is true. Include enough surrounding text to make old unique.",
                json!({"type":"object","properties":{"path":{"type":"string"},"old":{"type":"string"},
                "new":{"type":"string"},"replace_all":{"type":"boolean"}},
                "required":["path","old","new"],"additionalProperties":false}),
            ),
        };
        ToolSchema {
            name: self.name(),
            description,
            parameters,
        }
    }
}

#[derive(Clone)]
pub struct Registry {
    tools: Vec<Tool>,
    slots: Arc<Semaphore>,
    credentials: Arc<Vec<Credential>>,
    environment: Arc<Vec<(String, String)>>,
}
struct Credential {
    name: String,
    value: String,
}

pub enum Prepared {
    Echo(String),
    Shell {
        command: String,
        timeout_ms: u64,
    },
    Read {
        path: String,
        offset: usize,
        limit: usize,
    },
    Write {
        path: String,
        content: String,
    },
    Edit {
        path: String,
        old: String,
        new: String,
        replace_all: bool,
    },
}

/// Model-facing result text plus any full outputs that were truncated.
#[derive(Debug)]
pub struct Outcome {
    pub output: String,
    pub artifacts: Vec<(&'static str, Vec<u8>)>,
}
impl Outcome {
    fn text(output: String) -> Self {
        Self {
            output,
            artifacts: Vec::new(),
        }
    }
}

impl Registry {
    pub fn new(names: &str) -> Result<Self> {
        let mut tools = Vec::new();
        for name in names.split(',') {
            let tool = Tool::parse(name).ok_or(Error::new("unsupported_tool_set"))?;
            if tools.contains(&tool) {
                return fail("unsupported_tool_set");
            }
            if tool == Tool::Shell && !cfg!(unix) {
                return fail("shell_platform_unsupported");
            }
            tools.push(tool);
        }
        Ok(Self {
            tools,
            slots: Arc::new(Semaphore::new(16)),
            credentials: Arc::new(Vec::new()),
            environment: Arc::new(Vec::new()),
        })
    }
    /// Exact occurrences of these values are redacted from tool results and the
    /// named variables are removed from shell environments.
    /// Variables added to every shell child, such as the store and binary
    /// paths a bot needs to delegate through the same daemon.
    pub fn with_environment(mut self, environment: Vec<(String, String)>) -> Self {
        self.environment = Arc::new(environment);
        self
    }
    pub fn exclude_credentials(mut self, credentials: Vec<(String, String)>) -> Self {
        self.credentials = Arc::new(
            credentials
                .into_iter()
                .filter(|(_, value)| !value.is_empty())
                .map(|(name, value)| Credential { name, value })
                .collect(),
        );
        self
    }
    pub fn exclude_credential(self, name: &str, value: &str) -> Self {
        self.exclude_credentials(vec![(name.into(), value.into())])
    }
    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|t| t.name()).collect()
    }
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.iter().map(|t| t.schema()).collect()
    }
    fn redact(&self, mut text: String) -> String {
        for credential in self.credentials.iter() {
            if text.contains(&credential.value) {
                text = text.replace(&credential.value, "[REDACTED]");
            }
        }
        text
    }

    // Full access to this configured registry, within caller host permissions.
    // Validate arguments before recording intent so invalid calls never dispatch.
    pub fn prepare(&self, name: &str, args: &str) -> Result<Prepared> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Echo {
            text: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Shell {
            command: String,
            #[serde(default = "default_timeout")]
            timeout_ms: u64,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Read {
            path: String,
            #[serde(default = "one")]
            offset: usize,
            #[serde(default = "default_limit")]
            limit: usize,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Write {
            path: String,
            content: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Edit {
            path: String,
            old: String,
            new: String,
            #[serde(default)]
            replace_all: bool,
        }
        fn default_timeout() -> u64 {
            DEFAULT_SHELL_TIMEOUT_MS
        }
        fn one() -> usize {
            1
        }
        fn default_limit() -> usize {
            500
        }
        fn path(value: &str) -> Result<()> {
            if value.is_empty() || value.len() > 4096 || value.contains('\0') {
                return fail("invalid_path");
            }
            Ok(())
        }
        let tool = Tool::parse(name)
            .filter(|tool| self.tools.contains(tool))
            .ok_or(Error::new("unsupported_tool"))?;
        let invalid = |_| Error::new("invalid_tool_arguments");
        Ok(match tool {
            Tool::Echo => {
                let args: Echo = serde_json::from_str(args).map_err(invalid)?;
                if args.text.len() > PREVIEW_BYTES {
                    return fail("tool_output_limit");
                }
                Prepared::Echo(args.text)
            }
            Tool::Shell => {
                let args: Shell = serde_json::from_str(args).map_err(invalid)?;
                if args.command.is_empty()
                    || args.command.len() > 16 * 1024
                    || args.command.contains('\0')
                    || !(1..=MAX_SHELL_TIMEOUT_MS).contains(&args.timeout_ms)
                {
                    return fail("invalid_shell_arguments");
                }
                Prepared::Shell {
                    command: args.command,
                    timeout_ms: args.timeout_ms,
                }
            }
            Tool::Read => {
                let args: Read = serde_json::from_str(args).map_err(invalid)?;
                path(&args.path)?;
                if args.offset == 0 || !(1..=5000).contains(&args.limit) {
                    return fail("invalid_tool_arguments");
                }
                Prepared::Read {
                    path: args.path,
                    offset: args.offset,
                    limit: args.limit,
                }
            }
            Tool::Write => {
                let args: Write = serde_json::from_str(args).map_err(invalid)?;
                path(&args.path)?;
                if args.content.len() > WRITE_BYTES {
                    return fail("tool_input_limit");
                }
                Prepared::Write {
                    path: args.path,
                    content: args.content,
                }
            }
            Tool::Edit => {
                let args: Edit = serde_json::from_str(args).map_err(invalid)?;
                path(&args.path)?;
                if args.old.is_empty() || args.new.len() > WRITE_BYTES {
                    return fail("invalid_tool_arguments");
                }
                Prepared::Edit {
                    path: args.path,
                    old: args.old,
                    new: args.new,
                    replace_all: args.replace_all,
                }
            }
        })
    }

    pub async fn execute(&self, tool: Prepared, workspace: &Path) -> Result<Outcome> {
        match tool {
            Prepared::Echo(text) => Ok(Outcome::text(self.redact(text))),
            Prepared::Shell {
                command,
                timeout_ms,
            } => {
                let _slot = self
                    .slots
                    .acquire()
                    .await
                    .map_err(|_| Error::new("tool_scheduler_closed"))?;
                let (stdout, stderr, status) =
                    shell(&command, workspace, Duration::from_millis(timeout_ms), self).await?;
                let mut artifacts = Vec::new();
                let mut preview = |name: &'static str, bytes: Vec<u8>| {
                    // Reuse the pipe buffer for ordinary UTF-8 output. Invalid
                    // bytes still get the same lossy decoding as before.
                    let text = String::from_utf8(bytes).unwrap_or_else(|error| {
                        String::from_utf8_lossy(error.as_bytes()).into_owned()
                    });
                    let mut text = self.redact(text);
                    if text.len() <= PREVIEW_BYTES {
                        return text;
                    }
                    let shown = truncate(&text);
                    // read_to_end may have reserved beyond the output length.
                    // Do not retain that spare capacity while storage catches up.
                    text.shrink_to_fit();
                    artifacts.push((name, text.into_bytes()));
                    shown
                };
                let output =
                    json!({"stdout":preview("stdout", stdout),"stderr":preview("stderr", stderr),
                    "exit_code":status.code(),"success":status.success()})
                    .to_string();
                Ok(Outcome { output, artifacts })
            }
            Prepared::Read {
                path,
                offset,
                limit,
            } => {
                let bytes = read_bounded(&resolve(workspace, &path)).await?;
                let text = String::from_utf8_lossy(&bytes);
                let total = text.lines().count();
                let mut output = String::new();
                let mut shown = 0;
                for (index, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
                    // Reserve space for continuation/line-count notices. Check
                    // lengths before copying a potentially multi-megabyte line.
                    let prefix = format!("{:>6}\t", index + 1);
                    let entry_bytes = prefix.len() + line.len() + 1;
                    if output.len() + entry_bytes > PREVIEW_BYTES - 128 {
                        if shown == 0 {
                            return crate::fail_with(
                                "read_line_too_long",
                                format!(
                                    "line {} exceeds the read page budget; use a byte-oriented tool to inspect it",
                                    index + 1
                                ),
                            );
                        }
                        output.push_str(&format!(
                            "[truncated at line {}; continue with offset={}]\n",
                            index + 1,
                            index + 1
                        ));
                        break;
                    }
                    output.push_str(&prefix);
                    output.push_str(line);
                    output.push('\n');
                    shown += 1;
                }
                if offset > total && total > 0 {
                    output = format!("[offset {offset} is past the last line {total}]\n");
                } else if shown < total - (offset - 1).min(total) {
                    output.push_str(&format!(
                        "[showing lines {offset}-{} of {total}]\n",
                        offset + shown - 1
                    ));
                }
                Ok(Outcome::text(self.redact(output)))
            }
            Prepared::Write { path, content } => {
                let target = resolve(workspace, &path);
                if let Some(parent) = target.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                files::write(&target, content.as_bytes()).await?;
                Ok(Outcome::text(format!(
                    "wrote {} bytes to {}",
                    content.len(),
                    path
                )))
            }
            Prepared::Edit {
                path,
                old,
                new,
                replace_all,
            } => {
                let target = resolve(workspace, &path);
                let bytes = read_bounded(&target).await?;
                let text = String::from_utf8(bytes).map_err(|_| Error::new("file_not_utf8"))?;
                let count = text.matches(&old).count();
                if count == 0 {
                    return fail("edit_target_not_found");
                }
                if count > 1 && !replace_all {
                    return Err(Error::with(
                        "edit_target_ambiguous",
                        format!("{count} occurrences; add context or set replace_all"),
                    ));
                }
                let replacements = if replace_all { count } else { 1 };
                let expanded = new
                    .len()
                    .checked_mul(replacements)
                    .and_then(|added| (text.len() - old.len() * replacements).checked_add(added))
                    .filter(|size| *size <= FILE_BYTES)
                    .ok_or(Error::new("tool_output_limit"))?;
                let updated = if replace_all {
                    text.replace(&old, &new)
                } else {
                    text.replacen(&old, &new, 1)
                };
                debug_assert_eq!(updated.len(), expanded);
                files::write(&target, updated.as_bytes()).await?;
                Ok(Outcome::text(format!(
                    "replaced {count} occurrence{} in {path}",
                    if count == 1 { "" } else { "s" }
                )))
            }
        }
    }
}

fn resolve(workspace: &Path, path: &str) -> PathBuf {
    workspace.join(path)
}

fn boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Head and tail of an oversized output, with the omission stated.
fn truncate(text: &str) -> String {
    let half = PREVIEW_BYTES / 2;
    let head = &text[..boundary(text, half)];
    let tail_start = boundary(text, text.len() - half);
    let omitted = text.len() - head.len() - (text.len() - tail_start);
    format!(
        "{head}\n[... {omitted} bytes omitted; full output retained as an artifact ...]\n{}",
        &text[tail_start..]
    )
}

async fn bounded_read(mut pipe: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut pipe)
        .take(ARTIFACT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > ARTIFACT_BYTES {
        return fail("shell_output_limit");
    }
    Ok(bytes)
}

#[cfg(unix)]
async fn shell(
    command: &str,
    workspace: &Path,
    timeout: Duration,
    registry: &Registry,
) -> Result<(Vec<u8>, Vec<u8>, std::process::ExitStatus)> {
    use std::process::Stdio;
    use tokio::process::{Child, Command};
    struct ProcessGroup {
        child: Child,
        group: i32,
    }
    impl Drop for ProcessGroup {
        fn drop(&mut self) {
            // Only the group created for this command. Also runs if the turn's
            // future is cancelled. Escaped sessions require external isolation.
            if self.group > 1 {
                unsafe {
                    libc::kill(-self.group, libc::SIGKILL);
                }
            }
        }
    }
    let mut process = Command::new("/bin/sh");
    for credential in registry.credentials.iter() {
        process.env_remove(&credential.name);
    }
    for (name, value) in registry.environment.iter() {
        process.env(name, value);
    }
    let child = process
        .arg("-c")
        .arg(command)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let group = child.id().ok_or(Error::new("shell_spawn_failed"))? as i32;
    let mut owned = ProcessGroup { child, group };
    let stdout = owned
        .child
        .stdout
        .take()
        .ok_or(Error::new("shell_pipe_failed"))?;
    let stderr = owned
        .child
        .stderr
        .take()
        .ok_or(Error::new("shell_pipe_failed"))?;
    let result = tokio::time::timeout(timeout, async {
        tokio::try_join!(bounded_read(stdout), bounded_read(stderr), async {
            owned.child.wait().await.map_err(Error::from)
        })
    })
    .await
    .unwrap_or_else(|_| fail("shell_timeout"));
    // Finish cleanup before returning a timeout/overflow. Drop also covers
    // cancellation, with Tokio responsible for reaping its killed child.
    unsafe {
        libc::kill(-group, libc::SIGKILL);
    }
    let _ = owned.child.wait().await;
    owned.group = 0;
    result
}

#[cfg(not(unix))]
async fn shell(
    _: &str,
    _: &Path,
    _: Duration,
    _: &Registry,
) -> Result<(Vec<u8>, Vec<u8>, std::process::ExitStatus)> {
    fail("shell_platform_unsupported")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_output_keeps_head_and_tail_on_char_boundaries() {
        let text = format!(
            "{}é{}",
            "a".repeat(PREVIEW_BYTES),
            "z".repeat(PREVIEW_BYTES)
        );
        let shown = truncate(&text);
        assert!(shown.starts_with("aaaa"));
        assert!(shown.ends_with("zzzz"));
        assert!(shown.contains("bytes omitted"));
        assert!(shown.len() < text.len());
    }
    #[test]
    fn registries_reject_unknown_and_duplicate_tools() {
        assert!(Registry::new("echo,read,write,edit").is_ok());
        assert!(Registry::new("echo,echo").is_err());
        assert!(Registry::new("mcp").is_err());
        assert_eq!(Registry::new("read").unwrap().names(), ["read"]);
    }
}
