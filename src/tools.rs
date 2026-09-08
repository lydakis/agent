//! Registered tools share an execution budget; no per-bot worker or shell exists.
use crate::{Error, Result, fail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::Semaphore,
};

#[derive(Clone)]
pub struct Registry {
    shell: bool,
    slots: Arc<Semaphore>,
    credential: Option<Arc<Credential>>,
}
struct Credential {
    name: String,
    value: String,
}
impl Credential {
    fn redact(&self, text: &str) -> String {
        if self.value.is_empty() {
            return text.to_owned();
        }
        text.replace(&self.value, "[REDACTED]")
    }
}
pub enum Prepared {
    Echo(String),
    Shell { command: String, timeout_ms: u64 },
}
impl Registry {
    pub fn new(names: &str) -> Result<Self> {
        if !matches!(names, "echo" | "echo,shell") {
            return fail("unsupported_tool_set");
        }
        if names == "echo,shell" && !cfg!(unix) {
            return fail("shell_platform_unsupported");
        }
        Ok(Self {
            shell: names == "echo,shell",
            slots: Arc::new(Semaphore::new(16)),
            credential: None,
        })
    }
    pub fn exclude_credential(mut self, name: &str, value: &str) -> Self {
        self.credential = Some(Arc::new(Credential {
            name: name.into(),
            value: value.into(),
        }));
        self
    }
    pub fn names(&self) -> Vec<&'static str> {
        if self.shell {
            vec!["echo", "shell"]
        } else {
            vec!["echo"]
        }
    }
    pub fn schemas(&self) -> Value {
        let mut tools = vec![
            json!({"type":"function","name":"echo","description":"Return the supplied text unchanged.",
            "parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}}),
        ];
        if self.shell {
            tools.push(json!({"type":"function","name":"shell","description":"Run a noninteractive /bin/sh command in the bot workspace. No background jobs. stdout/stderr each limited to 32 KiB; timeout at most 30 seconds.",
                "parameters":{"type":"object","properties":{"command":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1,"maximum":30000}},"required":["command"],"additionalProperties":false}}));
        }
        json!(tools)
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
        fn default_timeout() -> u64 {
            10_000
        }
        match name {
            "echo" => {
                let args: Echo = serde_json::from_str(args)?;
                if args.text.len() > 64 * 1024 {
                    return fail("tool_output_limit");
                }
                Ok(Prepared::Echo(args.text))
            }
            "shell" if self.shell => {
                let args: Shell = serde_json::from_str(args)?;
                if args.command.is_empty()
                    || args.command.len() > 16 * 1024
                    || args.command.contains('\0')
                    || !(1..=30_000).contains(&args.timeout_ms)
                {
                    return fail("invalid_shell_arguments");
                }
                Ok(Prepared::Shell {
                    command: args.command,
                    timeout_ms: args.timeout_ms,
                })
            }
            _ => fail("unsupported_tool"),
        }
    }
    pub async fn execute(&self, tool: Prepared, workspace: &Path) -> Result<String> {
        match tool {
            Prepared::Echo(text) => Ok(match &self.credential {
                Some(credential) => credential.redact(&text),
                None => text,
            }),
            Prepared::Shell {
                command,
                timeout_ms,
            } => {
                let _slot = self
                    .slots
                    .acquire()
                    .await
                    .map_err(|_| Error("tool_scheduler_closed".into()))?;
                shell(
                    &command,
                    workspace,
                    Duration::from_millis(timeout_ms),
                    self.credential.as_deref(),
                )
                .await
            }
        }
    }
}

async fn bounded_read(mut pipe: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut pipe)
        .take(32 * 1024 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > 32 * 1024 {
        return fail("shell_output_limit");
    }
    Ok(bytes)
}

#[cfg(unix)]
async fn shell(
    command: &str,
    workspace: &Path,
    timeout: Duration,
    credential: Option<&Credential>,
) -> Result<String> {
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
    if let Some(credential) = credential {
        process.env_remove(&credential.name);
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
    let group = child.id().ok_or(Error("shell_spawn_failed".into()))? as i32;
    let mut owned = ProcessGroup { child, group };
    let stdout = owned
        .child
        .stdout
        .take()
        .ok_or(Error("shell_pipe_failed".into()))?;
    let stderr = owned
        .child
        .stderr
        .take()
        .ok_or(Error("shell_pipe_failed".into()))?;
    let result = tokio::time::timeout(timeout, async {
        let (stdout, stderr, status) =
            tokio::try_join!(bounded_read(stdout), bounded_read(stderr), async {
                owned.child.wait().await.map_err(Error::from)
            })?;
        let clean = |bytes: &[u8]| {
            let text = String::from_utf8_lossy(bytes);
            match credential {
                Some(credential) => credential.redact(&text),
                None => text.into_owned(),
            }
        };
        Ok::<_, Error>(
            json!({"stdout":clean(&stdout),"stderr":clean(&stderr),
            "exit_code":status.code(),"success":status.success()})
            .to_string(),
        )
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
async fn shell(_: &str, _: &Path, _: Duration, _: Option<&Credential>) -> Result<String> {
    fail("shell_platform_unsupported")
}
