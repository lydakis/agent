//! The models a provider lists for the credentials the daemon holds. Asked
//! only when a client wants them, never on the turn path, and kept for five
//! minutes, as Codex and OpenCode keep their catalogs. The daemon does not
//! consult the list to run anything: a turn runs whatever model it names.
use super::{Provider, aws, connection_error, error_body, sanitize_error};
use crate::{Error, Result, codec::Family, fail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

const KEEP: Duration = Duration::from_secs(300);
const DEADLINE: Duration = Duration::from_secs(10);
/// OpenRouter's catalog is the largest known, a few hundred KiB.
const LIMIT: usize = 8 * 1024 * 1024;
/// SHA-256 of an empty body, for a signer that signs the payload.
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Default)]
pub struct Listing(tokio::sync::Mutex<Option<(Instant, Arc<Vec<Value>>)>>);

impl Provider {
    /// `{id, name?, context_tokens?, output_tokens?}` per model, ids without
    /// the provider prefix. Callers asking at once share one request.
    pub async fn models(&self) -> Result<Arc<Vec<Value>>> {
        let mut kept = self.listing.0.lock().await;
        if let Some((at, models)) = kept.as_ref()
            && at.elapsed() < KEEP
        {
            return Ok(models.clone());
        }
        let key = self.key.clone();
        let session = match &self.login {
            Some(login) => Some(login.current()?),
            None => None,
        };
        let token = session.as_ref().map(|s| s.token.clone()).or(key);
        let fetched = tokio::time::timeout(DEADLINE, self.fetch_models(session.as_ref()))
            .await
            .unwrap_or_else(|_| fail("provider_connection_timeout"))
            .map_err(|error| sanitize_error(error, token.as_deref()))?;
        let models = Arc::new(fetched);
        *kept = Some((Instant::now(), models.clone()));
        Ok(models)
    }

    async fn fetch_models(&self, session: Option<&super::login::Session>) -> Result<Vec<Value>> {
        // The listing sits beside the completion route: `.../v1/models`.
        let mut url = self.url.clone();
        let base = url
            .path()
            .rsplit_once('/')
            .map_or("", |(base, _)| base)
            .to_owned();
        url.set_path(&format!("{base}/models"));
        if self.family == Family::Anthropic && self.aws.is_none() {
            url.set_query(Some("limit=1000"));
        }
        let (client, _lease) = self.transport.lease();
        let key = session.map(|s| &s.token).or(self.key.as_ref());
        let mut http = client.get(url.clone()).header("accept", "application/json");
        http = match (self.family, key) {
            (Family::Responses, Some(key)) => http.bearer_auth(key),
            (Family::Anthropic, key) => {
                let http = http.header("anthropic-version", "2023-06-01");
                match key {
                    Some(key) => http.header("x-api-key", key),
                    None => http,
                }
            }
            (Family::Responses, None) => http,
        };
        if let Some(session) = session {
            http = http.header("chatgpt-account-id", &session.account);
        }
        if let Some(aws) = &self.aws {
            let keys = aws.current().await?;
            let payload = if aws.signs_payload() {
                EMPTY_SHA256
            } else {
                aws::UNSIGNED_PAYLOAD
            };
            for (name, value) in aws.sign(&keys, "GET", &url, std::time::SystemTime::now(), payload)
            {
                http = http.header(name, value);
            }
        }
        let response = http.send().await.map_err(connection_error)?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            let detail = error_body(response).await.and_then(|body| body.detail);
            return Err(Error {
                code: format!("provider_http_{status}"),
                detail,
            });
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(connection_error)?;
            if chunk.len() > LIMIT - body.len() {
                return fail("provider_response_limit");
            }
            body.extend_from_slice(&chunk);
        }
        let listed: Value = serde_json::from_slice(&body)
            .map_err(|_| Error::with("invalid_provider_response", "model listing"))?;
        parse(&listed)
    }
}

/// The shapes providers answer with: `data` (OpenAI, Anthropic, OpenRouter,
/// Bedrock Mantle) or `models` (the ChatGPT Codex backend, whose `hide` and
/// `none` entries its own picker leaves out too).
fn parse(listed: &Value) -> Result<Vec<Value>> {
    let number = |entry: &Value, keys: &[&str]| {
        keys.iter()
            .find_map(|key| entry.pointer(key).and_then(Value::as_u64))
    };
    let entries = match (listed["data"].as_array(), listed["models"].as_array()) {
        (Some(data), _) => data,
        (None, Some(models)) => models,
        _ => return Err(Error::with("invalid_provider_response", "model listing")),
    };
    Ok(entries
        .iter()
        .filter(|entry| entry["visibility"].as_str().is_none_or(|v| v == "list"))
        .filter_map(|entry| {
            let id = entry["id"].as_str().or(entry["slug"].as_str())?;
            let mut model = json!({"id": id});
            let name = entry["display_name"].as_str().or(entry["name"].as_str());
            if let Some(name) = name.filter(|name| *name != id) {
                model["name"] = name.into();
            }
            if let Some(tokens) = number(
                entry,
                &["/max_input_tokens", "/context_length", "/context_window"],
            ) {
                model["context_tokens"] = tokens.into();
            }
            if let Some(tokens) = number(
                entry,
                &["/max_tokens", "/top_provider/max_completion_tokens"],
            ) {
                model["output_tokens"] = tokens.into();
            }
            Some(model)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::parse;
    use serde_json::json;

    #[test]
    fn each_provider_shape_reads_as_ids_with_what_it_says_about_them() {
        let anthropic = json!({"data":[{"type":"model","id":"claude-sonnet-5",
            "display_name":"Claude Sonnet 5","max_input_tokens":1000000,"max_tokens":128000}],
            "has_more":false});
        assert_eq!(
            parse(&anthropic).unwrap(),
            [json!({"id":"claude-sonnet-5","name":"Claude Sonnet 5",
                "context_tokens":1000000,"output_tokens":128000})]
        );
        let openai = json!({"object":"list","data":[{"id":"gpt-6-luna","object":"model",
            "created":1,"owned_by":"openai"}]});
        assert_eq!(parse(&openai).unwrap(), [json!({"id":"gpt-6-luna"})]);
        let openrouter = json!({"data":[{"id":"vendor/model","name":"Vendor: Model",
            "context_length":200000,"top_provider":{"max_completion_tokens":64000}}]});
        assert_eq!(
            parse(&openrouter).unwrap(),
            [json!({"id":"vendor/model","name":"Vendor: Model",
                "context_tokens":200000,"output_tokens":64000})]
        );
        let codex = json!({"models":[
            {"slug":"gpt-6-luna","display_name":"GPT-6-Luna","context_window":272000,"visibility":"list"},
            {"slug":"internal","display_name":"Internal","visibility":"hide"}]});
        assert_eq!(
            parse(&codex).unwrap(),
            [json!({"id":"gpt-6-luna","name":"GPT-6-Luna","context_tokens":272000})]
        );
        assert_eq!(
            parse(&json!({"error":"x"})).unwrap_err().code,
            "invalid_provider_response"
        );
    }
}
