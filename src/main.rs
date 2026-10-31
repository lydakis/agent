mod cli;
mod client;
mod client_path;
mod server;
mod sign_in;
use agent_runtime::{Error, Result, fail_with};

// Machine-readable temporary startup conflict. The CLI waits for the owner
// using this exit status, never by parsing a shared human-readable log.
const DAEMON_OWNERSHIP_CONFLICT: i32 = 75;

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
    let Some(args) = cli::prepare(args)? else {
        return Ok(0);
    };
    if args[0] == "login" {
        return sign_in::main(&args[1..]);
    }
    if args[0] == "serve" {
        SERVING.store(true, std::sync::atomic::Ordering::Relaxed);
        let config = configuration(&args[1..])?;
        runtime()?.block_on(server::run(config))?;
        Ok(0)
    } else {
        client::main(args)
    }
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
    let mut idle_exit = None;
    let mut stall_timeout = None;
    let mut shell_excludes = Vec::new();
    let (mut redact_fields, mut redact_blocks) = (Vec::new(), Vec::new());
    let (mut protected, mut protected_existing) = (Vec::new(), Vec::new());
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
            // `prepare` has checked each range.
            "--stall-timeout" => stall_timeout = Some(cli::duration_secs(flag, value)?),
            "--idle-exit" => {
                let seconds = cli::duration_secs(flag, value)?;
                idle_exit = (seconds > 0).then_some(seconds);
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
        stall_timeout,
        idle_exit,
        shell_excludes,
        redact_fields,
        redact_blocks,
        protected,
        protected_existing,
    })
}
