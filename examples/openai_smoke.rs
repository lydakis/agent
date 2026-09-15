//! Explicit paid probe: at most four calls, each capped at 2,048 output tokens.
//! Uses only synthetic text and echo; never prints prompts, outputs, or secrets.
use agent_runtime::{
    Error, Result,
    codec::Family,
    history::History,
    provider::{Delta, Provider, Request, Transport},
    tools::Registry,
};
use serde_json::{Value, json};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run().await {
        // Provider details can echo request data. Keep this probe's output safe.
        eprintln!("{}", json!({"status":"failed","code":error.code}));
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let family = Family::Responses;
    let key = std::env::var("OPENAI_API_KEY").map_err(|_| Error::new("missing_key"))?;
    let tools = Registry::new("echo")?;
    let provider = Provider::new(
        Transport::new(64)?,
        family,
        "https://api.openai.com/v1",
        Some(key),
        &tools.schemas(),
    )?
    .with_max_output_tokens(2048)?;
    let mut history = History::default();
    history.append(
        family
            .user_item(
                "Call echo exactly once with text amber-otter-17. Then repeat its result verbatim.",
            )?
            .into(),
    )?;
    let mut calls = 0;
    let mut encrypted = 0;
    let mut input_tokens = 0;
    let mut output_tokens = 0;
    let mut stage = 0;
    for round in 0..4 {
        let mut text = String::new();
        let completion = provider
            .complete(
                Request {
                    model: "gpt-5-mini-2025-08-07",
                    instructions: "Follow the request exactly. Be concise.",
                    reasoning: Some("low"),
                    history: &history,
                },
                |delta| {
                    if let Delta::Text(part) = delta {
                        text.push_str(&part);
                    }
                    std::future::ready(Ok(()))
                },
            )
            .await?;
        let usage = completion.usage.ok_or(Error::new("missing_usage"))?;
        input_tokens += usage.input_tokens;
        output_tokens += usage.output_tokens;
        for item in completion.items {
            let value: Value = serde_json::from_slice(&item)?;
            if value["type"] == "reasoning"
                && value["encrypted_content"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
            {
                encrypted += 1;
            }
            history.append(item)?;
        }
        if !completion.calls.is_empty() {
            if stage != 0 || calls != 0 || completion.calls.len() != 1 {
                return Err(Error::new("unexpected_tool_calls"));
            }
            for call in completion.calls {
                let prepared = tools.prepare(&call.name, &call.arguments)?;
                let result = tools.execute(prepared, &std::env::current_dir()?).await?;
                if call.name != "echo" || result.output != "amber-otter-17" {
                    return Err(Error::new("incorrect_tool_result"));
                }
                history.append(
                    family
                        .tool_result_item(&call.call_id, &result.output)?
                        .into(),
                )?;
                calls += 1;
            }
            continue;
        }
        if calls != 1 || !text.contains("amber-otter-17") {
            return Err(Error::new("incorrect_completion"));
        }
        if stage == 1 {
            if encrypted == 0 {
                return Err(Error::new("missing_encrypted_reasoning"));
            }
            println!(
                "{}",
                json!({"status":"ok", "model":"gpt-5-mini-2025-08-07",
                "requests":round+1,"tool_calls":calls,"encrypted_reasoning_items":encrypted,
                "input_tokens":input_tokens,"output_tokens":output_tokens,
                "history_continuation":true})
            );
            return Ok(());
        }
        // Reconstruct native history from serialized items, as a storage reader
        // does. This probes API continuation, not daemon crash recovery.
        let items = history.items();
        history = History::default();
        for item in items {
            history.append(item.to_vec().into())?;
        }
        history.append(
            family
                .user_item(
                    "Without calling any tool, repeat the exact text echoed in the previous turn.",
                )?
                .into(),
        )?;
        stage = 1;
    }
    Err(Error::new("request_budget_exhausted"))
}
