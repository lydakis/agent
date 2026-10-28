//! Web search, the way each provider offers it and Codex uses it. OpenAI's
//! own endpoint takes a `web.run` function the daemon answers through its
//! `alpha/search`, as Codex's GPT-6 models search (openai/codex 154e873,
//! codex-rs/ext/web-search): their "responses lite" mode refuses hosted
//! tools. The ChatGPT plan refuses both.
//! Bedrock's Responses endpoint hosts `web_search` for the GPT models it
//! serves; Anthropic hosts its own server tool, which Bedrock does not.
//! Any other gateway offers none, and a bot there is refused the tool.
use super::{Provider, connection_error, error_body, sanitize_error};
use crate::{Error, Result, codec::Family, fail_with};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::LazyLock;
use std::time::Duration;

/// The namespace and function a `web.run` call comes back naming.
pub const RUN_NAMESPACE: &str = "web";
pub const RUN_NAME: &str = "run";

/// Codex's `web.run` description and its commands' schema, without the
/// literature search only some models are offered.
const RUN_DESCRIPTION: &str = include_str!("../tools/web_run.md");
const RUN_PARAMETERS: &str = include_str!("../tools/web_run.json");

/// The most a search's answer may take; Codex bounds it by the model's
/// tool-output budget, 10,000 tokens for every GPT-6 model.
const MAX_OUTPUT_TOKENS: u64 = 10_000;
const MAX_RESPONSE: usize = 1024 * 1024;
/// A search that opens pages takes seconds; one that has not answered in
/// two minutes is failed to the model, which may try again.
const DEADLINE: Duration = Duration::from_secs(120);

/// The `web.run` function, as a Responses namespace and as a plain tool
/// named `web_search` for another family.
static RUN: LazyLock<(String, String)> = LazyLock::new(|| {
    let parameters: Value = serde_json::from_str(RUN_PARAMETERS).expect("web.run schema");
    (
        json!({"type":"namespace","name":RUN_NAMESPACE,
            "description":"Tools in the web namespace.",
            "tools":[{"type":"function","name":RUN_NAME,"description":RUN_DESCRIPTION,
                "strict":false,"parameters":parameters}]})
        .to_string(),
        json!({"name":"web_search","description":RUN_DESCRIPTION,"input_schema":parameters})
            .to_string(),
    )
});

/// How a provider searches the web for a model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Search {
    /// It offers no search.
    None,
    /// The daemon runs `web.run` through the provider's `alpha/search`.
    Run,
    /// The provider runs a tool it hosts, declared as this entry.
    Hosted(&'static str),
}

impl Search {
    /// The entry the request's `tools` takes for it, in `family`.
    pub fn entry(self, family: Family) -> Option<&'static str> {
        match (self, family) {
            (Search::None, _) => None,
            (Search::Run, Family::Responses) => Some(&RUN.0),
            (Search::Run, Family::Anthropic) => Some(&RUN.1),
            (Search::Hosted(entry), _) => Some(entry),
        }
    }
}

/// Whether a Claude model takes the search that filters its results with
/// code before they reach the context: Claude 4.6 and later, but Haiku.
fn filters_results(model: &str) -> bool {
    let model = super::claude_model(model);
    !super::legacy_thinking(model) && !model.starts_with("claude-haiku")
}

impl Provider {
    /// Whether this is OpenAI's own endpoint on an API key, whose
    /// `alpha/search` Codex calls. A ChatGPT plan login is refused there
    /// (401 `hardened_oauth_rule_missing`, observed 2026-10-28) and takes no
    /// hosted tools, so a plan bot has no search.
    fn openai(&self) -> bool {
        self.login.is_none() && self.url.host_str() == Some("api.openai.com")
    }

    /// How this provider searches for `model`.
    pub fn search(&self, model: &str) -> Search {
        match self.family {
            Family::Responses if self.openai() => Search::Run,
            // Bedrock serves indexed search only; asking for the live web
            // takes a permission most roles lack.
            Family::Responses if self.bedrock => {
                Search::Hosted(r#"{"type":"web_search","external_web_access":false}"#)
            }
            Family::Responses => Search::None,
            Family::Anthropic if self.bedrock => Search::None,
            Family::Anthropic if filters_results(model) => {
                Search::Hosted(r#"{"type":"web_search_20260209","name":"web_search"}"#)
            }
            Family::Anthropic => {
                Search::Hosted(r#"{"type":"web_search_20250305","name":"web_search"}"#)
            }
        }
    }

    /// Whether any model here can search.
    pub fn searches(&self) -> bool {
        self.search("") != Search::None
    }

    /// Run a `web.run` call: `commands` are its arguments, `session` keeps
    /// one bot's searches together. Returns the text the model reads.
    pub async fn run_search(&self, model: &str, session: &str, commands: &str) -> Result<String> {
        self.run_search_inner(model, session, commands)
            .await
            .map_err(|error| sanitize_error(error, self.key.as_deref()))
    }

    async fn run_search_inner(&self, model: &str, session: &str, commands: &str) -> Result<String> {
        if self.search(model) != Search::Run {
            return fail_with("web_search_unavailable", "this provider runs no web.run");
        }
        self.search_at(self.search_url(), model, session, commands)
            .await
    }

    /// `alpha/search` beside the provider's `responses`.
    fn search_url(&self) -> reqwest::Url {
        let mut url = self.url.clone();
        let base = url.path().trim_end_matches("/responses").to_owned();
        url.set_path(&format!("{base}/alpha/search"));
        url
    }

    async fn search_at(
        &self,
        url: reqwest::Url,
        model: &str,
        session: &str,
        commands: &str,
    ) -> Result<String> {
        let commands: Value = serde_json::from_str(commands)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| Error::new("invalid_tool_arguments"))?;
        let body = json!({"id":session,"model":model,"commands":commands,
            "settings":{"allowed_callers":["direct"],"external_web_access":true},
            "max_output_tokens":MAX_OUTPUT_TOKENS});
        let admission = self.admit().await?;
        let (client, _lease) = self.transport.lease();
        let mut http = client
            .post(url)
            .timeout(DEADLINE)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&body)?);
        if let Some(key) = &self.key {
            http = http.bearer_auth(key);
        }
        let response = http.send().await.map_err(connection_error)?;
        drop(admission);
        let status = response.status().as_u16();
        if !response.status().is_success() {
            // A refusal is the search's failure: the model reads why.
            let detail = error_body(response).await.and_then(|body| body.detail);
            return Err(Error {
                code: format!("provider_http_{status}"),
                detail,
                facts: None,
            });
        }
        let mut stream = response.bytes_stream();
        let mut read = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(connection_error)?;
            if read.len() + chunk.len() > MAX_RESPONSE {
                return fail_with("output_limit", "the search answered more than 1 MiB");
            }
            read.extend_from_slice(&chunk);
        }
        #[derive(serde::Deserialize)]
        struct Answer {
            output: String,
        }
        serde_json::from_slice::<Answer>(&read)
            .map(|answer| answer.output)
            .map_err(|_| Error::with("invalid_search_response", "no output in the answer"))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Items, Request, Transport, login};
    use super::*;
    use serde_json::value::RawValue;
    use std::sync::Arc;

    fn provider(family: Family, base: &str, key: Option<&str>) -> Provider {
        Provider::new(
            Transport::new(64, 1).unwrap(),
            family,
            base,
            key.map(str::to_owned),
        )
        .unwrap()
    }

    /// The `tools` a request for `model` with these tools and search sends.
    fn tools(provider: &Provider, model: &str, tools: &str, web_search: bool) -> Value {
        let tools = RawValue::from_string(tools.into()).unwrap();
        let prefix = provider
            .prefix(&Request {
                model,
                instructions: "i",
                reasoning: None,
                tools: &tools,
                allow_tool_calls: true,
                web_search,
                fallbacks: false,
                cache_key: None,
                items: Items::empty(),
                chain: None,
                route: None,
                sent: None,
            })
            .unwrap();
        let body: Value = serde_json::from_slice(&[&prefix[..], b"]}"].concat()).unwrap();
        body.get("tools").cloned().unwrap_or(Value::Null)
    }

    const ECHO: &str = r#"[{"type":"function","name":"echo"}]"#;

    #[test]
    fn openai_keys_take_codexs_web_run_beside_the_bots_tools() {
        let openai = provider(Family::Responses, "https://api.openai.com/v1", Some("k"));
        assert_eq!(openai.search("gpt-6-sol"), Search::Run);
        let sent = tools(&openai, "gpt-6-sol", ECHO, true);
        assert_eq!(sent[0], json!({"type":"function","name":"echo"}));
        assert_eq!(sent[1]["type"], "namespace");
        assert_eq!(sent[1]["name"], RUN_NAMESPACE);
        assert_eq!(sent[1]["tools"][0]["name"], RUN_NAME);
        assert_eq!(sent[1]["tools"][0]["parameters"]["type"], "object");
        assert_eq!(sent.as_array().unwrap().len(), 2);
        // Without it, or with no other tools, the list is otherwise unchanged.
        assert_eq!(
            tools(&openai, "gpt-6-sol", ECHO, false),
            json!([{"type":"function","name":"echo"}])
        );
        assert_eq!(
            tools(&openai, "gpt-6-sol", "[]", true)[0]["name"],
            RUN_NAMESPACE
        );
        assert_eq!(
            openai.search_url().as_str(),
            "https://api.openai.com/v1/alpha/search"
        );
    }

    #[test]
    fn hosts_offer_their_own_search_or_none() {
        let bedrock_gpt = provider(
            Family::Responses,
            "https://bedrock-mantle.us-east-1.api.aws/openai/v1",
            None,
        );
        assert_eq!(
            tools(&bedrock_gpt, "openai.gpt-6-sol", "[]", true),
            json!([{"type":"web_search","external_web_access":false}])
        );
        let anthropic = provider(Family::Anthropic, "https://api.anthropic.com/v1", Some("k"));
        for (model, version) in [
            ("claude-opus-5-5", "web_search_20260209"),
            ("claude-sonnet-4-6", "web_search_20260209"),
            ("claude-haiku-4-5", "web_search_20250305"),
            ("claude-sonnet-4-5", "web_search_20250305"),
        ] {
            let sent = tools(&anthropic, model, ECHO, true);
            assert_eq!(
                sent[1],
                json!({"type":version,"name":"web_search"}),
                "{model}"
            );
        }
        // A bot with no tools of its own still sends the search.
        assert_eq!(
            tools(&anthropic, "claude-opus-5-5", "[]", true)
                .as_array()
                .unwrap()
                .len(),
            1
        );
        for none in [
            provider(
                Family::Anthropic,
                "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1",
                None,
            ),
            provider(Family::Responses, "https://openrouter.ai/api/v1", Some("k")),
            provider(
                Family::Responses,
                "http://api.openai.com.example.test/v1",
                Some("k"),
            ),
        ] {
            assert_eq!(none.search("claude-opus-5-5"), Search::None);
            assert!(!none.searches());
            assert_eq!(
                tools(&none, "claude-opus-5-5", ECHO, true)
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }
    }

    #[test]
    fn a_plan_login_offers_no_search() {
        let dir = login::tests::temp("search-login");
        let path = dir.join("chatgpt.json");
        login::tests::save(&path, "synthetic-token");
        let login = Arc::new(login::Login::open(&path, None).unwrap());
        let plan = provider(Family::Responses, "https://api.openai.com/v1", None)
            .with_login(login)
            .unwrap();
        assert!(!plan.searches());
        let sent = tools(&plan, "gpt-6-sol", ECHO, true);
        assert_eq!(sent.as_array().unwrap().len(), 1);
        assert_eq!(sent[0]["name"], "agent");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// One fake `alpha/search` answer: the request it read, as text, and
    /// the provider's result.
    async fn answered(status: &str, body: &str, commands: &str) -> (String, Result<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/alpha/search", listener.local_addr().unwrap());
        let reply = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let server = tokio::spawn(async move {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(Duration::from_secs(5), listener.accept()).await
            else {
                return String::new();
            };
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let n = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..n]);
                let text = String::from_utf8_lossy(&request);
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .and_then(|n| n.parse::<usize>().ok())
                        .unwrap_or(0);
                    if rest.len() >= length || n == 0 {
                        break;
                    }
                }
            }
            socket.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        let openai = provider(
            Family::Responses,
            "https://api.openai.com/v1",
            Some("synthetic-key"),
        );
        let result = openai
            .search_at(url.parse().unwrap(), "gpt-6-sol", "bot-session", commands)
            .await
            .map_err(|error| sanitize_error(error, Some("synthetic-key")));
        (server.await.unwrap(), result)
    }

    #[tokio::test]
    async fn a_web_run_posts_its_commands_and_returns_the_output() {
        let (request, result) = answered(
            "200 OK",
            r#"{"output":"1. Result"}"#,
            r#"{"search_query":[{"q":"rust"}]}"#,
        )
        .await;
        assert_eq!(result.unwrap(), "1. Result");
        assert!(request.starts_with("POST /v1/alpha/search "), "{request}");
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer synthetic-key")
        );
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            body,
            json!({"id":"bot-session","model":"gpt-6-sol",
                "commands":{"search_query":[{"q":"rust"}]},
                "settings":{"allowed_callers":["direct"],"external_web_access":true},
                "max_output_tokens":10000})
        );
    }

    #[tokio::test]
    async fn a_refused_or_shapeless_search_fails_to_the_model() {
        let (_, refused) = answered(
            "403 Forbidden",
            r#"{"error":{"message":"no search here"}}"#,
            r#"{"search_query":[]}"#,
        )
        .await;
        assert_eq!(refused.unwrap_err().code, "provider_http_403");
        let (_, shapeless) = answered("200 OK", r#"{"results":[]}"#, r#"{}"#).await;
        assert_eq!(shapeless.unwrap_err().code, "invalid_search_response");
        // Arguments that are not an object never leave the daemon.
        let openai = provider(Family::Responses, "https://api.openai.com/v1", Some("k"));
        let error = openai
            .search_at(openai.search_url(), "gpt-6-sol", "s", "[]")
            .await
            .unwrap_err();
        assert_eq!(error.code, "invalid_tool_arguments");
        let anthropic = provider(Family::Anthropic, "https://api.anthropic.com/v1", Some("k"));
        let error = anthropic
            .run_search("claude-opus-5-5", "s", "{}")
            .await
            .unwrap_err();
        assert_eq!(error.code, "web_search_unavailable");
    }
}
