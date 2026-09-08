use agent_runtime::{Error, Result, fail, history::History, output::Output, provider::Provider};
use serde::Deserialize;
use serde_json::json;
use tokio::task::JoinSet;

#[derive(Deserialize)]
struct Workload {
    concurrency: usize,
    turns: usize,
    chunks: usize,
    chunk_bytes: usize,
    history_bytes: usize,
}

pub async fn run(output: Output) -> Result<()> {
    let config: Workload = serde_json::from_str(
        &std::env::var("AGENT_BENCH_WORKLOAD").map_err(|_| Error("missing_workload".into()))?,
    )?;
    if config.concurrency == 0
        || config.concurrency > 4096
        || config.turns == 0
        || config.turns > 10000
        || config.chunks == 0
        || config.chunk_bytes == 0
        || config.history_bytes > 1024 * 1024
        || config
            .chunks
            .checked_mul(config.chunk_bytes)
            .is_none_or(|n| n > 256 * 1024)
    {
        return fail("workload_limit");
    }
    let port: u16 = std::env::var("AGENT_BENCH_PORT")
        .map_err(|_| Error("missing_port".into()))?
        .parse()
        .map_err(|_| Error("invalid_port".into()))?;
    if port == 0 {
        return fail("invalid_port");
    }
    let provider = Provider::new(
        &format!("http://127.0.0.1:{port}/v1"),
        "bench-model",
        "Complete the synthetic benchmark turn.",
        json!([]),
        None,
    )?;
    let mut jobs = JoinSet::new();
    output.send(json!({"event":"ready"})).await?;
    for agent in 0..config.concurrency {
        let provider = provider.clone();
        let output = output.clone();
        let (turns, chunks, chunk_bytes, history_bytes) = (
            config.turns,
            config.chunks,
            config.chunk_bytes,
            config.history_bytes,
        );
        jobs.spawn(async move {
            let mut history = History::default();
            for turn in 0..turns {
                let agent = agent.to_string();
                let turn = turn.to_string();
                output
                    .send(json!({"event":"turn_start","agent":agent,"turn":turn}))
                    .await?;
                history.user(&format!(
                    "BENCH agent={agent} turn={turn}\n{}",
                    "x".repeat(history_bytes)
                ))?;
                let mut bytes = 0;
                let mut seq = 0;
                let response = provider
                    .complete(&history, |text| {
                        let output = output.clone();
                        let agent = agent.clone();
                        let turn = turn.clone();
                        let valid = text.bytes().all(|b| b == b'x');
                        bytes += text.len();
                        let first = seq;
                        seq = bytes / chunk_bytes;
                        let (seq, bytes) = (seq, bytes);
                        async move {
                            if !valid || bytes > chunks * chunk_bytes {
                                return fail("invalid_benchmark_output");
                            }
                            for index in first..seq {
                                output
                                    .send(json!({"event":"chunk","agent":agent,"turn":turn,
                                "seq":index,"bytes":chunk_bytes}))
                                    .await?;
                            }
                            Ok(())
                        }
                    })
                    .await?;
                if bytes != chunks * chunk_bytes || !response.calls.is_empty() {
                    return fail("incomplete_benchmark");
                }
                for item in response.items {
                    history.append(item)?;
                }
                output
                    .send(json!({"event":"turn_end","agent":agent,"turn":turn}))
                    .await?;
            }
            Ok::<_, Error>(())
        });
    }
    while let Some(result) = jobs.join_next().await {
        result.map_err(|_| Error("task_failed".into()))??;
    }
    Ok(())
}
