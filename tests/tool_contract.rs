#![cfg(unix)]
use agent_runtime::tools::Registry;
use serde_json::{Value, json};

#[tokio::test]
async fn shell_preserves_exit_status_and_separate_outputs() {
    let tools = Registry::new("echo,shell").unwrap();
    let cwd = std::env::temp_dir();
    let prepared = tools
        .prepare(
            "shell",
            &json!({"command":"printf out; printf err >&2; exit 7","timeout_ms":1000}).to_string(),
        )
        .unwrap();
    let output: Value =
        serde_json::from_str(&tools.execute(prepared, &cwd).await.unwrap()).unwrap();
    assert_eq!(output["stdout"], "out");
    assert_eq!(output["stderr"], "err");
    assert_eq!(output["exit_code"], 7);
    assert!(
        Registry::new("echo")
            .unwrap()
            .prepare("shell", r#"{"command":"true"}"#)
            .is_err()
    );
    assert!(
        tools
            .prepare("shell", r#"{"command":"true","timeout_ms":0}"#)
            .is_err()
    );
}

#[tokio::test]
async fn shell_timeout_and_output_overflow_are_bounded() {
    let tools = Registry::new("echo,shell").unwrap();
    for (command, timeout, error) in [
        ("sleep 10", 30, "shell_timeout"),
        ("yes x", 1000, "shell_output_limit"),
    ] {
        let prepared = tools
            .prepare(
                "shell",
                &json!({"command":command,"timeout_ms":timeout}).to_string(),
            )
            .unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tools.execute(prepared, &std::env::temp_dir()),
        )
        .await
        .unwrap();
        assert_eq!(result.unwrap_err().0, error);
    }
}

#[tokio::test]
async fn known_credential_text_is_redacted_before_tool_result_serialization() {
    let sentinel = "synthetic-\"test-value";
    let tools = Registry::new("echo,shell")
        .unwrap()
        .exclude_credential("AGENT_TEST_FAKE_KEY", sentinel);
    let echo = tools
        .prepare("echo", &json!({"text":sentinel}).to_string())
        .unwrap();
    assert!(tools.execute(echo, &std::env::temp_dir()).await.unwrap() == "[REDACTED]");
    let shell = tools.prepare("shell", &json!({"command": "printf '%s' 'synthetic-\"test-value'; printf '%s' 'synthetic-\"test-value' >&2"}).to_string()).unwrap();
    let output = tools.execute(shell, &std::env::temp_dir()).await.unwrap();
    let result: Value = serde_json::from_str(&output).unwrap();
    assert!(result["stdout"] == "[REDACTED]");
    assert!(result["stderr"] == "[REDACTED]");
}
