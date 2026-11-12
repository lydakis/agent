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
    use agent_runtime::sandbox::{Daemon, Mode, Policy};
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
        return fail_with("usage", "--bot-id, --store and --socket go with --bot");
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
    // What a daemon this CLI starts keeps read-only in every root: a
    // checkout's git folder, agents' settings, cloud keys.
    let names = |names: &[&str]| names.iter().map(|name| name.to_string()).collect();
    let daemon = Daemon {
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
        // And what the daemon walls off, as it would for a bot.
        walled: (agent_client::config::run())
            .and_then(|run| std::fs::canonicalize(run).ok())
            .into_iter()
            .collect(),
        // And shows the folders a daemon this CLI starts shows from copies.
        owned: agent_client::OWN_COPIES
            .map(std::path::PathBuf::from)
            .to_vec(),
        ..Daemon::default()
    };
    agent_runtime::sandbox::available()?;
    let mode = match writable.is_empty() {
        true => Mode::ReadOnly,
        false => Mode::WorkspaceWrite,
    };
    let policy = Policy::new(Some(mode), None, &writable, false, &daemon)?
        .ok_or(Error::new("sandbox_unavailable"))?;
    run_in(Some(&policy), program, &rest, None)
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
        let held = match environment {
            Some(_) => None,
            None => Some(agent_runtime::sandbox::hold(policy).await),
        };
        let status = match agent_runtime::sandbox::command(policy, program, rest, false) {
            Ok(mut child) => {
                given(&mut child);
                child.status().await.map_err(Error::from)
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
    let mut idle_exit = None;
    let mut await_owner = None;
    let mut stall_timeout = None;
    let mut shell_excludes = Vec::new();
    let (mut redact_fields, mut redact_blocks) = (Vec::new(), Vec::new());
    let (mut protected, mut protected_existing) = (Vec::new(), Vec::new());
    let mut protected_paths = Vec::new();
    let mut walled = Vec::new();
    let mut own_copies = Vec::new();
    let mut windows = Vec::new();
    let mut speeds = Vec::new();
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
            "--redact-field" | "--redact-block" if value.trim().is_empty() => {
                return fail_with("usage", format!("{flag} needs a name"));
            }
            "--redact-field" => redact_fields.push(value.clone()),
            "--redact-block" => redact_blocks.push(value.clone()),
            "--protect" | "--protect-existing"
                if value.is_empty() || value.contains('/') || value == "." || value == ".." =>
            {
                return fail_with("usage", format!("{flag} needs a name in a folder"));
            }
            "--protect" => protected.push(value.clone()),
            "--protect-path" => protected_paths.push(value.into()),
            "--wall" => walled.push(value.into()),
            "--own-copy" if !std::path::Path::new(value).is_absolute() => {
                return fail_with("usage", "--own-copy needs an absolute folder");
            }
            "--own-copy" => own_copies.push(value.into()),
            "--context-window" => windows.push(value.as_str()),
            "--speeds" => speeds.push(value.as_str()),
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
            // `prepare` has checked each range.
            "--stall-timeout" => stall_timeout = Some(cli::duration_secs(flag, value)?),
            "--idle-exit" => {
                let seconds = cli::duration_secs(flag, value)?;
                idle_exit = (seconds > 0).then_some(seconds);
            }
            "--await-owner" => await_owner = Some(cli::duration_secs(flag, value)?),
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
        redact_fields,
        redact_blocks,
        protected,
        protected_existing,
        protected_paths,
        walled,
        own_copies,
        context_windows: std::sync::Arc::new(agent_runtime::provider::Windows::parse(windows)?),
        speeds: std::sync::Arc::new(agent_runtime::provider::Speeds::parse(speeds)?),
        fallbacks: std::sync::Arc::new(agent_runtime::provider::Fallbacks::parse(fallbacks)?),
        models,
        provider_config,
        max_depth,
    })
}
