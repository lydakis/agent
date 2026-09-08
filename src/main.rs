mod benchmark;
mod server;
use agent_runtime::{Error, Result, output::Output};

fn main() {
    if let Err(error) = run() {
        eprintln!("agent: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!("agent-runtime {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let config = if args.first().is_some_and(|a| a == "serve") {
        Some(configuration(&args[1..])?)
    } else if args == ["benchmark"] {
        None
    } else {
        return agent_runtime::fail(
            "usage: agent serve --store PATH --base-url URL --model MODEL [--key-env NAME] [--tools echo|echo,shell] | benchmark | --version",
        );
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (output, writer) = Output::stdout();
    let result = runtime.block_on(async move {
        if let Some(config) = config {
            server::run(config, output).await
        } else {
            let result = benchmark::run(output.clone()).await;
            if let Err(error) = &result {
                let code = match error.0.as_str() {
                    "provider_connection_timeout"
                    | "provider_connection_failed"
                    | "provider_stream_failed"
                    | "missing_completion"
                    | "output_closed" => error.0.clone(),
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
        }
    });
    drop(runtime); // Release aborted tasks and their output handles on failure.
    // Drain accepted output before exit. Reader disconnection is a failure.
    writer
        .join()
        .map_err(|_| Error("output_worker_failed".into()))??;
    result
}

fn configuration(args: &[String]) -> Result<server::Configuration> {
    let mut values = std::collections::HashMap::new();
    for pair in args.chunks(2) {
        if pair.len() != 2
            || !["--store", "--base-url", "--model", "--key-env", "--tools"]
                .contains(&pair[0].as_str())
            || values.insert(pair[0].as_str(), pair[1].clone()).is_some()
        {
            return agent_runtime::fail("invalid_serve_arguments");
        }
    }
    Ok(server::Configuration {
        store: values
            .remove("--store")
            .ok_or(Error("missing_store".into()))?
            .into(),
        base_url: values
            .remove("--base-url")
            .ok_or(Error("missing_base_url".into()))?,
        model: values
            .remove("--model")
            .ok_or(Error("missing_model".into()))?,
        key_env: values.remove("--key-env"),
        tools: values.remove("--tools").unwrap_or_else(|| "echo".into()),
    })
}
