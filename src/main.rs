mod cli;
mod client;
mod client_path;
mod server;
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
            eprintln!("agent: {error}");
            std::process::exit(match error.code.as_str() {
                "usage" => 2,
                "store_already_owned" | "socket_already_owned" => DAEMON_OWNERSHIP_CONFLICT,
                _ => 1,
            });
        }
    }
}

fn run() -> Result<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") if args.len() == 1 => {
            println!("agent-runtime {}", env!("CARGO_PKG_VERSION"));
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
        None => return fail_with("usage", "a command is required; use agent --help"),
        _ => {}
    }
    let Some(args) = cli::prepare(args)? else {
        return Ok(0);
    };
    if args[0] == "serve" {
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
    let mut max_active = None;
    let mut max_connecting = None;
    let mut max_pending = None;
    let mut max_pending_bytes = None;
    let mut max_output_tokens = None;
    let mut idle_exit = None;
    let mut context_bytes = None;
    let mut context_items = None;
    let mut retain_turns = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .ok_or(Error::with("usage", format!("{flag} needs a value")))?;
        match flag.as_str() {
            "--store" => store = Some(value.into()),
            "--socket" => socket = Some(value.into()),
            "--provider" => providers.push(server::ProviderSpec::parse(value)?),
            "--max-processes"
            | "--max-active"
            | "--max-connecting"
            | "--max-pending"
            | "--max-pending-bytes" => {
                let parsed: usize = value
                    .parse()
                    .map_err(|_| Error::with("usage", format!("{flag} needs an integer")))?;
                match flag.as_str() {
                    "--max-processes" => max_processes = Some(parsed),
                    "--max-active" => max_active = Some(parsed),
                    "--max-pending" => max_pending = Some(parsed),
                    "--max-pending-bytes" => max_pending_bytes = Some(parsed),
                    _ => max_connecting = Some(parsed),
                }
            }
            "--context-bytes" | "--context-items" => {
                let parsed: usize = value.parse().ok().filter(|n| *n > 0).ok_or(Error::with(
                    "usage",
                    format!("{flag} needs a positive integer"),
                ))?;
                if flag == "--context-bytes" {
                    context_bytes = Some(parsed);
                } else {
                    context_items = Some(parsed);
                }
            }
            "--max-output-tokens" => {
                max_output_tokens = Some(value.parse::<u32>().ok().filter(|n| *n > 0).ok_or(
                    Error::with("usage", "--max-output-tokens needs a positive integer"),
                )?)
            }
            "--retain-turns" => {
                retain_turns = Some(value.parse::<usize>().ok().filter(|n| *n > 0).ok_or(
                    Error::with("usage", "--retain-turns needs a positive integer"),
                )?)
            }
            "--idle-exit" => {
                let seconds: u64 = value
                    .parse()
                    .map_err(|_| Error::with("usage", "--idle-exit needs seconds (0 disables)"))?;
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
        max_active,
        max_connecting,
        max_pending,
        max_pending_bytes,
        max_output_tokens,
        idle_exit,
        context_bytes,
        context_items,
        retain_turns,
    })
}
