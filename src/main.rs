mod benchmark;
mod client;
mod client_path;
mod server;
use agent_runtime::{Error, Result, fail_with, output::Output};

// Machine-readable temporary startup conflict. The CLI waits for the owner
// using this exit status, never by parsing a shared human-readable log.
const DAEMON_OWNERSHIP_CONFLICT: i32 = 75;

const USAGE: &str = "usage:
  agent run [options] PROMPT...      run a turn and stream its events as JSONL (starts the daemon if needed)
                                     --bot NAME continues that bot; --new --bot NAME creates it; no --bot makes a fresh one
  agent follow --bot NAME [--after N] replay then stream a bot's events as JSONL
  agent fork --source NAME --checkpoint N --bot NAME [--workspace DIR]
  agent interrupt --bot NAME
  agent ls | agent shutdown
  agent serve --store PATH [--socket PATH] --provider SPEC... [--model P/M] [--tools LIST]
  agent benchmark | agent --version
options: --store PATH --model PROVIDER/MODEL --provider SPEC --tools LIST --workspace DIR
         --bot NAME --instructions TEXT --instructions-file F --reasoning low|medium|high
         --request-id ID --new --detach (submit and return a turn handle) --pretty (human rendering instead of JSONL) --no-spawn
provider SPEC: NAME[=FAMILY[,BASE_URL[,KEY_ENV]]]; families: responses, anthropic";

fn main() {
    match run() {
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
        Some("--version") => {
            println!("agent-runtime {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Some("serve") => {
            let config = configuration(&args[1..])?;
            runtime()?.block_on(server::run(config))?;
            Ok(0)
        }
        Some("benchmark") => {
            let runtime = runtime()?;
            let (output, writer) = Output::stdout();
            let result = runtime.block_on(async move {
                let result = benchmark::run(output.clone()).await;
                if let Err(error) = &result {
                    let code = match error.code.as_str() {
                        "provider_connection_timeout"
                        | "provider_connection_failed"
                        | "provider_stream_failed"
                        | "missing_completion"
                        | "output_closed" => error.code.clone(),
                        code if code
                            .strip_prefix("provider_connection_os_")
                            .is_some_and(|n| n.parse::<i32>().is_ok()) =>
                        {
                            code.into()
                        }
                        _ => "benchmark_failed".into(),
                    };
                    let _ = output
                        .send(serde_json::json!({"event":"diagnostic","stage":"benchmark","code":code}))
                        .await;
                }
                result
            });
            drop(runtime); // Release aborted tasks and their output handles on failure.
            // Drain accepted output before exit. Reader disconnection is a failure.
            writer
                .join()
                .map_err(|_| Error::new("output_worker_failed"))??;
            result.map(|_| 0)
        }
        Some("run" | "follow" | "fork" | "interrupt" | "ls" | "shutdown") => client::main(args),
        _ => fail_with("usage", USAGE),
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
    let mut model = None;
    let mut tools = None;
    let mut instructions = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .ok_or(Error::with("usage", format!("{flag} needs a value")))?;
        match flag.as_str() {
            "--store" => store = Some(value.into()),
            "--socket" => socket = Some(value.into()),
            "--provider" => providers.push(server::ProviderSpec::parse(value)?),
            "--model" => model = Some(value.clone()),
            "--tools" => tools = Some(value.clone()),
            "--instructions" => instructions = Some(value.clone()),
            _ => return fail_with("usage", USAGE),
        }
    }
    if providers.is_empty() {
        return fail_with("usage", "serve needs at least one --provider");
    }
    Ok(server::Configuration {
        store: store.ok_or(Error::with("usage", "serve needs --store"))?,
        socket,
        providers,
        model,
        instructions,
        tools: tools.unwrap_or_else(|| "echo".into()),
    })
}
