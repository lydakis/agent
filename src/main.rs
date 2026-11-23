mod cli;
mod client;
mod client_path;
#[cfg(test)]
mod doubling;
mod server;
mod sign_in;
use agent_runtime::{Error, Result, fail_with};

// Machine-readable temporary startup conflict. The CLI waits for the owner
// using this exit status, never by parsing a shared human-readable log.
const DAEMON_OWNERSHIP_CONFLICT: i32 = 75;

/// `agent sandbox` could not run its command.
const SANDBOX_FAILED: i32 = 125;

#[cfg(feature = "heap-profile")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

// As Codex's binaries: jemalloc in the static Linux builds, where musl's
// allocator takes one lock for every thread. A heap profile counts with
// dhat's allocator instead.
#[cfg(all(
    not(feature = "heap-profile"),
    target_os = "linux",
    target_env = "musl",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() {
    // The profile is written when the profiler drops, so it must outlive
    // `run` and be dropped before the process exits.
    #[cfg(feature = "heap-profile")]
    let profiler = std::env::var("AGENT_HEAP_PROFILE").ok().map(|path| {
        dhat::Profiler::builder()
            .file_name(path)
            .trim_backtraces(Some(24))
            .build()
    });
    let result = run();
    #[cfg(feature = "heap-profile")]
    drop(profiler);
    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            report(&error);
            std::process::exit(match error.code.as_str() {
                // A missing provider is a missing flag, as usage errors are.
                "usage" | "provider_required" => 2,
                "store_already_owned" | "socket_already_owned" => DAEMON_OWNERSHIP_CONFLICT,
                _ => 1,
            });
        }
    }
}

/// A failure on stderr: one JSON object, `{"error", "detail", ...facts}`,
/// the daemon's own fields plus the CLI's `hint` in flags; `--pretty`
/// asks for a line a person reads instead. The detail is bounded as in a
/// daemon response, since it can echo an argument.
fn report(error: &Error) {
    let error = &Error {
        detail: error.detail.clone().map(agent_runtime::output::bounded),
        ..error.clone()
    };
    if cli::PRETTY.load(std::sync::atomic::Ordering::Relaxed) {
        match error
            .facts
            .as_ref()
            .and_then(|facts| facts.get("hint")?.as_str())
        {
            Some(hint) => eprintln!("agent: {error}; {hint}"),
            None => eprintln!("agent: {error}"),
        }
        return;
    }
    let mut report = serde_json::Map::new();
    report.insert("error".into(), error.code.clone().into());
    report.insert("detail".into(), error.detail.clone().into());
    if let Some(facts) = &error.facts {
        report.extend(facts.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    // A daemon names itself, so the launcher that started it can pick its
    // own line out of the log other starts append to; one write keeps the
    // line whole among theirs.
    if SERVING.load(std::sync::atomic::Ordering::Relaxed) {
        report.insert("pid".into(), std::process::id().into());
    }
    let mut line = serde_json::Value::Object(report).to_string();
    line.push('\n');
    let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
}

/// Set once this process is `agent serve`.
static SERVING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn run() -> Result<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") if args.len() == 1 => {
            match option_env!("AGENT_COMMIT") {
                Some(commit) => println!("agent-runtime {} ({commit})", env!("CARGO_PKG_VERSION")),
                None => println!("agent-runtime {}", env!("CARGO_PKG_VERSION")),
            }
            return Ok(0);
        }
        Some("--help" | "-h") if args.len() == 1 => {
            cli::help(None)?;
            return Ok(0);
        }
        Some("help") if args.len() <= 2 => {
            cli::help(args.get(1).map(String::as_str))?;
            return Ok(0);
        }
        // A sandboxed bot's write, which the daemon runs in the sandbox.
        Some(agent_runtime::sandbox::WRITE) if args.len() == 2 => {
            agent_runtime::sandbox::write(std::path::Path::new(&args[1]))?;
            return Ok(0);
        }
        // A sandboxed bot's patch, which the daemon runs in the sandbox.
        Some(agent_runtime::sandbox::PATCH) if args.len() == 1 => {
            agent_runtime::sandbox::patch()?;
            return Ok(0);
        }
        // A command whose bot names domains, bridged to the proxy, which
        // the daemon runs in the sandbox.
        Some(agent_runtime::sandbox::PROXY) => {
            return agent_runtime::sandbox::bridge(&args[1..]);
        }
        None => return fail_with("usage", "a command is required; use agent --help"),
        _ => {}
    }
    // `sandbox`'s own usage error is its failure too, and exits as one.
    let sandbox = args.first().is_some_and(|command| command == "sandbox");
    let args = match cli::prepare(args) {
        Ok(Some(args)) => args,
        Ok(None) => return Ok(0),
        Err(error) if sandbox => {
            report(&error);
            return Ok(SANDBOX_FAILED);
        }
        Err(error) => return Err(error),
    };
    if args[0] == "login" {
        return sign_in::main(&args[1..]);
    }
    if args[0] == "sandbox" {
        // Its own failure exits 125, as `env`, `timeout` and `docker run`
        // say theirs, so a caller reading the command's exit status, such
        // as a check where 1 says no, never takes it for the command's.
        return sandboxed(&args[1..]).or_else(|error| {
            report(&error);
            Ok(SANDBOX_FAILED)
        });
    }
    if args[0] == "tool" {
        return tool(&args[1..]);
    }
    if args[0] == "serve" {
        SERVING.store(true, std::sync::atomic::Ordering::Relaxed);
        // Before the runtime's threads: every one shares the limit.
        agent_runtime::open_files::raise();
        agent_runtime::footprint::start();
        let config = configuration(&args[1..])?;
        runtime()?.block_on(server::run(config))?;
        Ok(0)
    } else {
        client::main(args)
    }
}

/// `agent sandbox [--writable PATH]... -- COMMAND [ARGS...]`: the command
/// in the sandbox a bot's commands run in, writing only the folders named
/// (none: read-only), with the network off. Off, it reaches no Unix socket
/// either, so not the daemon's, nor a bot's, which a bot's sandbox hides
/// by the daemon's own knowledge of them. For a client that runs a
/// program in a folder an agent may write, which could run what the agent
/// left there: git runs a repository's hooks and the commands its config
/// names. As Codex's `codex sandbox`, with no daemon: the same sandbox,
/// run here. The agent's home stays read-only, as in every sandbox.
///
/// `--bot NAME` runs it as one of that bot's commands instead: in the
/// bot's sandbox, its folders, network and sockets, reaching the daemon
/// through the bot's own socket, which the daemon lends while it runs,
/// with the bot's variables. For a command a bot wrote that a client runs
/// outside the bot's turn, such as an automation's check: it does no more
/// than the bot could. Its own failure exits 125, as `env` and `timeout`
/// say theirs, the command's exit status otherwise.
fn sandboxed(args: &[String]) -> Result<i32> {
    use agent_runtime::sandbox::{Mode, Policy};
    let split = args
        .iter()
        .position(|arg| arg == "--")
        .ok_or(Error::with("usage", "sandbox needs -- before its command"))?;
    let (flags, command) = (&args[..split], &args[split + 1..]);
    let Some((program, rest)) = command.split_first() else {
        return fail_with("usage", "sandbox needs a command after --");
    };
    let mut writable = Vec::new();
    let (mut bot, mut bot_id, mut connection) = (None, None, Vec::new());
    let mut iter = flags.iter();
    while let Some(flag) = iter.next() {
        // As for any command on the bots: an error, not a daemon started,
        // when none runs.
        if flag == "--no-spawn" {
            connection.push(flag.clone());
            continue;
        }
        match (flag.as_str(), iter.next()) {
            // By its real path, which is what the sandbox sees.
            ("--writable", Some(path)) => match std::fs::canonicalize(path)
                .ok()
                .and_then(|real| real.to_str().map(str::to_owned))
            {
                Some(real) => writable.push(real),
                None => {
                    return fail_with("usage", format!("--writable {path} is not a folder here"));
                }
            },
            ("--bot", Some(name)) => bot = Some(name.clone()),
            ("--bot-id", Some(id)) => {
                bot_id = Some(
                    (id.parse::<i64>()).map_err(|_| Error::with("usage", "--bot-id is an id"))?,
                );
            }
            ("--store" | "--socket", Some(path)) => connection.extend([flag.clone(), path.clone()]),
            _ => return fail_with("usage", format!("sandbox does not accept {flag}")),
        }
    }
    if bot.is_none() && (bot_id.is_some() || !connection.is_empty()) {
        return fail_with(
            "usage",
            "--bot-id, --store, --socket and --no-spawn go with --bot",
        );
    }
    if bot.is_some() && !writable.is_empty() {
        return fail_with(
            "usage",
            "--writable conflicts with --bot, whose sandbox says what it writes",
        );
    }
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
    if let Some(bot) = bot {
        // The lend lasts while this holds its connection: until the command
        // is done. The daemon says what the bot's commands get, as it built it
        // for them: its sandbox and its whole environment.
        let lent = client::lend(&connection, &bot, bot_id)?;
        let policy: Option<Policy> = serde_json::from_value(lent.answer["policy"].clone())
            .map_err(|_| {
                Error::with(
                    "sandbox_unavailable",
                    "the daemon's sandbox is not one known here",
                )
            })?;
        let environment: std::collections::BTreeMap<String, String> =
            serde_json::from_value(lent.answer["env"].clone()).map_err(|_| {
                Error::with("sandbox_unavailable", "the daemon sent no environment")
            })?;
        let status = run_in(policy.as_ref(), program, &rest, Some(&environment));
        drop(lent);
        return status;
    }
    let daemon = own_daemon()?;
    agent_runtime::sandbox::available()?;
    let mode = match writable.is_empty() {
        true => Mode::ReadOnly,
        false => Mode::WorkspaceWrite,
    };
    let policy = Policy::new(Some(mode), None, &writable, false, &daemon)?
        .ok_or(Error::new("sandbox_unavailable"))?;
    run_in(Some(&policy), program, &rest, None)
}

/// What a sandbox this CLI builds with no daemon keeps out of reach, as a
/// daemon it starts would for its bots.
fn own_daemon() -> Result<agent_runtime::sandbox::Daemon> {
    // What a daemon this CLI starts keeps read-only in every root: a
    // checkout's git folder, agents' settings, cloud keys.
    let names = |names: &[&str]| names.iter().map(|name| name.to_string()).collect();
    let listed = agent_client::config::load().map_err(|error| match error.split_once(": ") {
        Some((code, detail)) => Error::with(code, detail),
        None => Error::with("config_invalid", error),
    })?;
    Ok(agent_runtime::sandbox::Daemon {
        protected: names(&agent_client::PROTECTED),
        protected_existing: names(&agent_client::PROTECTED_EXISTING),
        // By its real path, which the sandbox sees, kept when it is not
        // there yet as well, so it may not be made.
        protected_paths: agent_client::config::home()
            .and_then(|home| {
                std::fs::canonicalize(&home).ok().or_else(|| {
                    Some(
                        std::fs::canonicalize(home.parent()?)
                            .ok()?
                            .join(home.file_name()?),
                    )
                })
            })
            .into_iter()
            .collect(),
        // And hides what a daemon this CLI starts hides from its bots: the
        // keys and the person's `[sandbox] unreadable` folders. Each made
        // first, as the daemon makes it, to be known by its real path.
        unreadable: (agent_client::hidden::folders(&listed).into_iter())
            .map(|folder| {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&folder)
                    .and_then(|()| std::fs::canonicalize(&folder))
                    .map_err(|error| {
                        Error::with("io_error", format!("{}: {error}", folder.display()))
                    })
            })
            .collect::<Result<_>>()?,
        // And what the daemon walls off, as it would for a bot.
        walled: (agent_client::config::run())
            .and_then(|run| std::fs::canonicalize(run).ok())
            .into_iter()
            .collect(),
        // And shows the folders a daemon this CLI starts shows from copies.
        owned: agent_client::OWN_COPIES
            .map(std::path::PathBuf::from)
            .to_vec(),
        copied_files: agent_client::own_copy_files(),
        // And reaches no daemon's socket and writes no daemon's store.
        known: agent_client::config::daemons(),
        // And writes through this program, as a daemon's bots write.
        helper: std::env::current_exe().ok(),
        ..agent_runtime::sandbox::Daemon::default()
    })
}

/// `agent tool [--writable PATH]... [--network] [--domain PATTERN[=deny]]...
/// [--workdir DIR] NAME ARGUMENTS`: one call of a bot's `shell`, `read`,
/// `write`, `edit` or `apply_patch`, with ARGUMENTS as the model gives
/// them, run by the daemon's own code in the sandbox `agent sandbox`
/// builds: writing only the folders named (none: read-only), the network
/// off unless `--network`, and then only the hosts `--domain` lets
/// through if it names any, relative paths from DIR (this folder unless named), and a
/// command with no `timeout_ms` run to its end, as a script's step. For a
/// client that lends this machine's shell and files to the agents of
/// another, as Codex's exec server does. Prints `{"output", "failed",
/// "parts"?}` as a daemon step's reply gives them; a call the tool refuses
/// is a result too, failed, as the model reads it. Exits 0 once it
/// printed one; its own failure is an error, exit 1 or 2.
fn tool(args: &[String]) -> Result<i32> {
    use agent_runtime::sandbox::{Mode, Policy};
    use agent_runtime::tools::Prepared;
    let (mut writable, mut network, mut workdir, mut held) = (Vec::new(), false, None, false);
    let mut domains = std::collections::BTreeMap::new();
    let mut positional = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match (arg.as_str(), arg.starts_with("--")) {
            ("--network", _) => network = true,
            ("--held", _) => held = true,
            ("--domain", _) => {
                let given = iter
                    .next()
                    .ok_or(Error::with("usage", "--domain needs a pattern"))?;
                let (pattern, access) = agent_runtime::sandbox::proxy::flag(given)
                    .map_err(|why| Error::with("usage", format!("--domain {why}")))?;
                domains.insert(pattern, access);
            }
            ("--writable", _) => {
                let path = iter
                    .next()
                    .ok_or(Error::with("usage", "--writable needs a folder"))?;
                match std::fs::canonicalize(path)
                    .ok()
                    .and_then(|real| real.to_str().map(str::to_owned))
                {
                    Some(real) => writable.push(real),
                    None => {
                        return fail_with(
                            "usage",
                            format!("--writable {path} is not a folder here"),
                        );
                    }
                }
            }
            ("--workdir", _) => {
                let dir = iter
                    .next()
                    .ok_or(Error::with("usage", "--workdir needs a folder"))?;
                workdir = Some(std::fs::canonicalize(dir).map_err(|_| {
                    Error::with("usage", format!("--workdir {dir} is not a folder here"))
                })?);
            }
            (flag, true) => return fail_with("usage", format!("tool does not accept {flag}")),
            (_, false) => positional.push(arg.clone()),
        }
    }
    let [name, arguments] = positional.as_slice() else {
        return fail_with("usage", "tool needs a tool's name and its arguments");
    };
    // `-`: the arguments are stdin's first line, past what one argument
    // of a command may hold; the rest of stdin, under --held, holds it.
    let arguments = match arguments.as_str() {
        "-" => {
            let mut line = String::new();
            std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line)?;
            line.trim_end_matches('\n').to_owned()
        }
        given => given.to_owned(),
    };
    let arguments = &arguments;
    if network && writable.is_empty() {
        return fail_with(
            "usage",
            "--network goes with --writable, as a sandbox's network goes with workspace-write",
        );
    }
    if !domains.is_empty() && !network {
        return fail_with("usage", "--domain goes with --network");
    }
    if !matches!(
        name.as_str(),
        "shell" | "read" | "write" | "edit" | "apply_patch"
    ) {
        return fail_with(
            "usage",
            format!("tool runs shell, read, write, edit or apply_patch, not {name}"),
        );
    }
    let workspace = match workdir {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };
    let registry = agent_runtime::tools::Registry::all()?.with_credentials(withheld()?);
    let daemon = own_daemon()?;
    let refused = |error: Error| {
        let output = serde_json::json!({"error":error.code,"detail":error.detail}).to_string();
        serde_json::json!({"output":output,"failed":true})
    };
    // As a script's step: a command with no time of its own runs to its
    // end, unless its caller stops it.
    let prepared = registry.prepare(name, arguments).map(|mut prepared| {
        if let Prepared::Shell { timeout_ms, .. } = &mut prepared
            && !arguments.contains("\"timeout_ms\"")
        {
            *timeout_ms = None;
        }
        prepared
    });
    let reply = match prepared {
        Err(error) => refused(error),
        // What runs past this call is not lent: a command runs to its end.
        Ok(
            Prepared::Shell {
                background: true, ..
            }
            | Prepared::Detach { .. }
            | Prepared::Terminal { .. },
        ) => refused(Error::with(
            "tool_refused",
            "a command on another machine runs in the foreground, to its end",
        )),
        // Its sandbox is the machine's: no call leaves it.
        Ok(Prepared::Shell { .. })
            if serde_json::from_str::<serde_json::Value>(arguments)
                .is_ok_and(|given| given["sandbox_permissions"] == "require_escalated") =>
        {
            refused(agent_runtime::tools::escalation_refused())
        }
        Ok(prepared) => {
            agent_runtime::sandbox::available()?;
            let sandbox = match &prepared {
                // A read happens here, outside any sandbox, kept out of
                // what no sandboxed bot reads.
                Prepared::Read { .. } => Policy::reads(&daemon),
                _ => {
                    let mode = match writable.is_empty() {
                        true => Mode::ReadOnly,
                        false => Mode::WorkspaceWrite,
                    };
                    Policy::new(Some(mode), None, &writable, network, &daemon)?
                        .ok_or(Error::new("sandbox_unavailable"))?
                        .with_domains(&domains)
                }
            };
            let ran = runtime()?.block_on(async {
                let ran = registry.execute(
                    prepared,
                    &workspace,
                    &[],
                    Some(std::sync::Arc::new(sandbox)),
                );
                if !held {
                    return Some(ran.await);
                }
                // Its stdin closed, the call is stopped: dropped, as the
                // daemon drops a call it stops, its commands with it.
                let (closed, closing) = tokio::sync::oneshot::channel::<()>();
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
                    let _ = closed.send(());
                });
                tokio::select! {
                    ran = ran => Some(ran),
                    _ = closing => None,
                }
            });
            let Some(ran) = ran else {
                return fail_with("stopped", "the call was stopped: its caller let it go");
            };
            match ran {
                Ok(mut outcome) => {
                    kept_whole(&mut outcome);
                    let mut reply =
                        serde_json::json!({"output":outcome.output,"failed":outcome.failed});
                    if !outcome.parts.is_empty() {
                        reply["parts"] = serde_json::json!(outcome.parts);
                    }
                    reply
                }
                Err(error) => refused(error),
            }
        }
    };
    println!("{reply}");
    Ok(0)
}

/// What no command `agent tool` runs is given, as a daemon this CLI starts
/// gives none: the variables `[shell_environment_policy] exclude` names,
/// the keys of `~/.agent/keys/env` and its providers' keys, whose values
/// are cut from what it prints too.
fn withheld() -> Result<agent_runtime::tools::Credentials> {
    let credentials = agent_runtime::tools::Credentials::default();
    let file =
        agent_client::config::load().map_err(|error| Error::with("config_invalid", error))?;
    let excludes = file.shell_environment_policy.excludes();
    for (name, _) in std::env::vars_os() {
        if let Some(name) = name.to_str()
            && (excludes.iter()).any(|pattern| agent_runtime::tools::name_matches(pattern, name))
        {
            credentials.withhold(name);
        }
    }
    for (name, value) in agent_client::env_file::for_daemon().unwrap_or_default() {
        credentials.withhold(&name);
        credentials.set(&name, &value);
    }
    // And each provider's key here, as a daemon withholds its providers':
    // the ones it knows by name, and those the person names.
    let named = client::environment_providers(&|name| std::env::var(name).ok(), &|| {
        Ok(file.provider.clone())
    })
    .unwrap_or_default();
    let keys = (named.iter())
        .filter_map(|spec| server::ProviderSpec::parse(spec).ok()?.key_env)
        .chain(
            [
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "OPENROUTER_API_KEY",
                "AWS_BEARER_TOKEN_BEDROCK",
            ]
            .map(String::from),
        );
    for name in keys {
        if let Ok(value) = std::env::var(&name) {
            credentials.withhold(&name);
            credentials.set(&name, &value);
        }
    }
    Ok(credentials)
}

/// Each stream `outcome` retained past what it shows, written whole to a
/// new file of its own, owner-only, in a new folder of the agent's own
/// (`~/.agent/outputs/tool`), which no sandbox writes, and named in its
/// output's `files`: there is no store here to keep it in, and the caller
/// reads it with `read` on this machine. One that can't be written is
/// left out.
fn kept_whole(outcome: &mut agent_runtime::tools::Outcome) {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let artifacts = std::mem::take(&mut outcome.artifacts);
    let Some(home) = agent_client::config::home().filter(|_| !artifacts.is_empty()) else {
        return;
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let folder = home
        .join("outputs/tool")
        .join(format!("{stamp}-{}", std::process::id()));
    let made = (std::fs::DirBuilder::new().recursive(true).mode(0o700))
        .create(folder.parent().unwrap_or(&folder))
        .and_then(|()| std::fs::DirBuilder::new().mode(0o700).create(&folder));
    if made.is_err() {
        return;
    }
    let mut files = Vec::new();
    for (stream, data) in &artifacts {
        let Some(whole) = data.whole() else {
            continue;
        };
        let path = folder.join(stream);
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, whole));
        if written.is_ok()
            && let Some(path) = path.to_str()
        {
            files.push(path.to_owned());
        }
    }
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&outcome.output)
        && value.is_object()
        && !files.is_empty()
    {
        value["files"] = serde_json::json!(files);
        outcome.output = value.to_string();
    }
}

/// `program` with `rest`, in `policy` when one is given, with `environment`
/// (a lent command's, whose sandbox the daemon holds) in place of this
/// one's when one is given: its exit status, as a shell
/// reports one a signal ended.
fn run_in(
    policy: Option<&agent_runtime::sandbox::Policy>,
    program: &str,
    rest: &[&str],
    environment: Option<&std::collections::BTreeMap<String, String>>,
) -> Result<i32> {
    // In place of this one's, keeping what the sandbox set itself (the
    // descriptor its bot's socket is handed by).
    let given = |command: &mut tokio::process::Command| {
        if let Some(environment) = environment {
            let own: Vec<(std::ffi::OsString, std::ffi::OsString)> = (command.as_std().get_envs())
                .filter_map(|(name, value)| Some((name.to_owned(), value?.to_owned())))
                .collect();
            command.env_clear().envs(environment).envs(own);
        }
    };
    let status = runtime()?.block_on(async {
        let Some(policy) = policy else {
            let mut command = tokio::process::Command::new(program);
            command.args(rest);
            given(&mut command);
            return command.status().await.map_err(Error::from);
        };
        // What the sandbox holds in place stays held while the command
        // runs, and what it left in place for it is gone before this exits,
        // as for a bot's command. A lent sandbox's the daemon holds, in the
        // list its own commands share, until this session closes.
        let mut held = match environment {
            Some(_) => None,
            None => Some(agent_runtime::sandbox::hold(policy).await),
        };
        let status = match agent_runtime::sandbox::command(policy, program, rest, false) {
            Ok((mut child, ended)) => {
                given(&mut child);
                let status = child.status().await.map_err(Error::from);
                // Its copy of the pipe goes first: the hold waits for its
                // end, once the sandbox is gone.
                drop(child);
                if let Some(held) = &mut held {
                    held.waits_for(ended);
                }
                status
            }
            Err(e) => Err(e),
        };
        if let Some(held) = held {
            held.release();
        }
        status
    })?;
    // A command a signal ended exits as a shell reports it.
    Ok(status.code().unwrap_or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        128 + status.signal().unwrap_or(0)
    }))
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

fn configuration(args: &[String]) -> Result<server::Configuration> {
    let mut store = None;
    let mut socket = None;
    let mut providers = Vec::new();
    let mut max_processes = None;
    let mut max_lasting = None;
    let mut max_active = None;
    let mut max_connecting = None;
    let mut max_pending = None;
    let mut max_pending_bytes = None;
    let mut max_input_bytes = None;
    let mut max_readers = None;
    let mut max_load = None;
    let mut max_depth = None;
    let mut max_restarts = Some(agent_runtime::store::RESTART_LIMIT);
    let mut idle_exit = None;
    let mut await_owner = None;
    let mut stall_timeout = None;
    let mut durability = agent_runtime::store::Durability::Full;
    let mut shell_excludes = Vec::new();
    let (mut protected, mut protected_existing) = (Vec::new(), Vec::new());
    let mut protected_paths = Vec::new();
    let mut walled = Vec::new();
    let mut unreadable = Vec::new();
    let mut own_copies = Vec::new();
    let mut own_copy_files = Vec::new();
    let mut fallbacks = Vec::new();
    let mut models = None;
    let mut provider_config = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .ok_or(Error::with("usage", format!("{flag} needs a value")))?;
        match flag.as_str() {
            "--store" => store = Some(value.into()),
            "--socket" => socket = Some(value.into()),
            "--provider" => providers.push(server::ProviderSpec::parse(value)?),
            "--shell-exclude" if value.trim().is_empty() => {
                return fail_with("usage", "--shell-exclude needs a name or pattern");
            }
            "--shell-exclude" => shell_excludes.push(value.clone()),
            "--protect" | "--protect-existing"
                if value.is_empty() || value.contains('/') || value == "." || value == ".." =>
            {
                return fail_with("usage", format!("{flag} needs a name in a folder"));
            }
            "--protect" => protected.push(value.clone()),
            "--protect-path" => protected_paths.push(value.into()),
            "--wall" => walled.push(value.into()),
            "--unreadable" => unreadable.push(value.into()),
            "--own-copy" if !std::path::Path::new(value).is_absolute() => {
                return fail_with("usage", "--own-copy needs an absolute folder");
            }
            "--own-copy" => own_copies.push(value.into()),
            "--own-copy-file" => match value.split_once('=') {
                Some((file, first)) if std::path::Path::new(file).is_absolute() => {
                    own_copy_files.push((file.into(), first.to_owned()))
                }
                _ => return fail_with("usage", "--own-copy-file needs FILE=FIRST, FILE absolute"),
            },
            "--fallbacks" => fallbacks.push(value.as_str()),
            "--models" => models = Some(value.into()),
            // Made whole here, so the file watched and reread on each edit is
            // the one named, with or without a folder in front of it.
            "--provider-config" => {
                provider_config = Some(std::path::absolute(value).unwrap_or_else(|_| value.into()))
            }
            "--protect-existing" => protected_existing.push(value.clone()),
            "--max-processes"
            | "--max-lasting"
            | "--max-active"
            | "--max-connecting"
            | "--max-pending"
            | "--max-pending-bytes"
            | "--max-input-bytes"
            | "--max-readers" => {
                let parsed: usize = value
                    .parse()
                    .map_err(|_| Error::with("usage", format!("{flag} needs an integer")))?;
                match flag.as_str() {
                    "--max-processes" => max_processes = Some(parsed),
                    "--max-lasting" => max_lasting = Some(parsed),
                    "--max-active" => max_active = Some(parsed),
                    "--max-pending" => max_pending = Some(parsed),
                    "--max-pending-bytes" => max_pending_bytes = Some(parsed),
                    "--max-input-bytes" => max_input_bytes = Some(parsed),
                    "--max-readers" if parsed == 0 => {
                        return fail_with("usage", "--max-readers needs at least 1");
                    }
                    "--max-readers" => max_readers = Some(parsed),
                    _ => max_connecting = Some(parsed),
                }
            }
            "--max-load" => {
                let load = cli::load(value)?;
                max_load = (load > 0.0).then_some(load);
            }
            "--max-depth" => {
                let depth: u64 = value
                    .parse()
                    .map_err(|_| Error::with("usage", format!("{flag} needs an integer")))?;
                max_depth = (depth > 0).then_some(depth);
            }
            "--restarts" => {
                let restarts: u32 = value
                    .parse()
                    .map_err(|_| Error::with("usage", format!("{flag} needs an integer")))?;
                max_restarts = (restarts > 0).then_some(restarts);
            }
            // `prepare` has checked each range.
            "--stall-timeout" => stall_timeout = Some(cli::duration_secs(flag, value)?),
            "--idle-exit" => {
                let seconds = cli::duration_secs(flag, value)?;
                idle_exit = (seconds > 0).then_some(seconds);
            }
            "--await-owner" => await_owner = Some(cli::duration_secs(flag, value)?),
            "--durability" => {
                durability = agent_runtime::store::Durability::parse(value)
                    .ok_or(Error::with("usage", "--durability needs full or normal"))?
            }
            _ => return fail_with("usage", format!("unknown option {flag}")),
        }
    }
    if providers.is_empty() {
        return fail_with("usage", "serve needs at least one --provider");
    }
    Ok(server::Configuration {
        store: store.ok_or(Error::with("usage", "serve needs --store"))?,
        socket,
        providers,
        max_processes,
        max_lasting,
        max_active,
        max_connecting,
        max_pending,
        max_pending_bytes,
        max_input_bytes,
        max_readers,
        max_load,
        stall_timeout,
        idle_exit,
        await_owner,
        shell_excludes,
        protected,
        protected_existing,
        protected_paths,
        walled,
        unreadable,
        own_copies,
        own_copy_files,
        fallbacks: std::sync::Arc::new(agent_runtime::provider::Fallbacks::parse(fallbacks)?),
        models,
        provider_config,
        max_depth,
        max_restarts,
        durability,
    })
}
