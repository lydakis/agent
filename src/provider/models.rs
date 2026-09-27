//! The models a provider lists for the credentials the daemon holds. Asked
//! only when a client wants them, never on the turn path, and kept for five
//! minutes, as Codex and OpenCode keep their catalogs. The daemon does not
//! consult the list to run anything: a turn runs whatever model it names.
use super::{Provider, aws, connection_error, error_body, sanitize_error};
use crate::{Error, Result, codec::Family, fail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const KEEP: Duration = Duration::from_secs(300);
const DEADLINE: Duration = Duration::from_secs(10);
/// Anthropic's pages hold 1,000 models; nobody lists fifty thousand.
const PAGES: usize = 50;
/// OpenRouter's catalog is the largest known, a few hundred KiB.
const LIMIT: usize = 8 * 1024 * 1024;
/// SHA-256 of an empty body, for a signer that signs the payload.
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// The ChatGPT backend refuses a listing without `client_version` and lists
/// only the models whose `minimal_client_version` it meets, as it does for
/// Codex (codex-rs/codex-api/src/endpoint/models.rs, 0896bf6).
const CODEX_CLIENT_VERSION: &str = "0.157.1";
/// Words in an id that name a model no text turn runs: embeddings, speech,
/// images, video, moderation, realtime and search previews, and models
/// served only as completions. A provider that states output modalities is taken at
/// its word instead.
const NOT_TEXT: &[&str] = &[
    "embedding",
    "embed",
    "whisper",
    "tts",
    "transcribe",
    "audio",
    "realtime",
    "dall",
    "image",
    "sora",
    "moderation",
    "search",
    "babbage",
    "davinci",
    "instruct",
];

/// The last answer, a refusal included, so a client asking again within
/// `KEEP` waits on neither the network nor a provider that is down. A
/// listing is kept only while every provider's together fit one reply, the
/// most a `provider_models` answer carries (`Transport::listed`).
#[derive(Default)]
pub struct Listing(tokio::sync::Mutex<Option<Kept>>);

struct Kept {
    at: Instant,
    listed: Listed,
    bytes: usize,
}

type Listed = Result<Arc<Vec<Value>>>;

impl Provider {
    /// `{id, name?, context_tokens?, output_tokens?}` per model, ids without
    /// the provider prefix. Callers asking at once share one request.
    pub async fn models(&self) -> Listed {
        let mut kept = self.listing.0.lock().await;
        if let Some(entry) = kept.as_ref()
            && entry.at.elapsed() < KEEP
        {
            return entry.listed.clone();
        }
        if let Some(entry) = kept.take() {
            self.transport
                .listed
                .fetch_sub(entry.bytes, Ordering::Relaxed);
        }
        let mut bytes = 0;
        let listed = tokio::time::timeout(DEADLINE, self.list_models())
            .await
            .unwrap_or_else(|_| fail("provider_connection_timeout"))
            .and_then(|models| {
                // Answered only if one reply could carry it.
                bytes = serde_json::to_vec(&models).map_or(usize::MAX, |json| json.len());
                match bytes <= crate::output::MAX_EVENT {
                    true => Ok(Arc::new(models)),
                    false => Err(Error::with(
                        "provider_models_limit",
                        format!("{} models, {bytes} bytes listed", models.len()),
                    )),
                }
            });
        if listed.is_err() {
            bytes = 0;
        }
        let held =
            self.transport
                .listed
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |held| {
                    held.checked_add(bytes)
                        .filter(|&total| total <= crate::output::MAX_EVENT)
                });
        if held.is_ok() {
            *kept = Some(Kept {
                at: Instant::now(),
                listed: listed.clone(),
                bytes,
            });
        }
        listed
    }

    /// A ChatGPT login refused with 401 is read again from disk and tried
    /// once more, as a completion is, since Codex may have rotated it.
    async fn list_models(&self) -> Result<Vec<Value>> {
        let Some(login) = &self.login else {
            return self
                .fetch_models(None)
                .await
                .map_err(|error| sanitize_error(error, self.key.as_deref()));
        };
        let session = login.current()?;
        match self.fetch_models(Some(&session)).await {
            Err(error) if error.code == "provider_http_401" && login.reload(&session)? => {
                let session = login.current()?;
                self.fetch_models(Some(&session))
                    .await
                    .map_err(|error| sanitize_error(error, Some(&session.token)))
            }
            listed => listed.map_err(|error| sanitize_error(error, Some(&session.token))),
        }
    }

    fn models_url(&self) -> reqwest::Url {
        // The listing sits beside the completion route: `.../v1/models`,
        // except on Bedrock Mantle, which lists every family at the host's
        // `/v1/models` (docs/BEDROCK.md).
        let mut url = self.url.clone();
        let base = match aws::endpoint(&url) {
            Some((_, "bedrock-mantle")) => "/v1".to_owned(),
            _ => url
                .path()
                .rsplit_once('/')
                .map_or("", |(base, _)| base)
                .to_owned(),
        };
        url.set_path(&format!("{base}/models"));
        if self.family == Family::Anthropic && self.aws.is_none() {
            url.set_query(Some("limit=1000"));
        }
        if self.login.is_some() {
            url.set_query(Some(&format!("client_version={CODEX_CLIENT_VERSION}")));
        }
        url
    }

    /// Bedrock Mantle lists every family at one address, and each route runs
    /// only its own: `anthropic.*` on Messages, `openai.*` on Responses.
    fn serves(&self, id: &str) -> bool {
        match (aws::endpoint(&self.url), self.family) {
            (Some((_, "bedrock-mantle")), Family::Anthropic) => id.starts_with("anthropic."),
            (Some((_, "bedrock-mantle")), Family::Responses) => id.starts_with("openai."),
            _ => true,
        }
    }

    /// Every page: Anthropic's listing pages with `has_more` and `last_id`,
    /// followed until done under the one deadline and body limit.
    async fn fetch_models(&self, session: Option<&super::login::Session>) -> Result<Vec<Value>> {
        let mut room = LIMIT;
        let mut entries = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..PAGES {
            let mut url = self.models_url();
            if let Some(after) = &after {
                url.query_pairs_mut().append_pair("after_id", after);
            }
            let mut page = self.fetch_page(session, url, &mut room).await?;
            let listed = match (page["data"].is_array(), page["models"].is_array()) {
                (true, _) => page["data"].take(),
                (false, true) => page["models"].take(),
                _ => return Err(Error::with("invalid_provider_response", "model listing")),
            };
            if let Value::Array(listed) = listed {
                entries.extend(listed);
            }
            if page["has_more"] != true {
                let mut models = parse(&json!({"data": entries}))?;
                models.retain(|model| model["id"].as_str().is_some_and(|id| self.serves(id)));
                return Ok(models);
            }
            match page["last_id"].as_str() {
                Some(last) if after.as_deref() != Some(last) => after = Some(last.to_owned()),
                _ => {
                    return Err(Error::with(
                        "invalid_provider_response",
                        "model listing has more pages but no new last_id",
                    ));
                }
            }
        }
        Err(Error::with(
            "provider_response_limit",
            format!("model listing runs past {PAGES} pages"),
        ))
    }

    async fn fetch_page(
        &self,
        session: Option<&super::login::Session>,
        url: reqwest::Url,
        room: &mut usize,
    ) -> Result<Value> {
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
        // Started under the same bound as a turn's request, released once
        // the headers arrive.
        let admission = self.admit().await?;
        let response = http.send().await.map_err(connection_error)?;
        drop(admission);
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
            *room = room
                .checked_sub(chunk.len())
                .ok_or(Error::new("provider_response_limit"))?;
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body)
            .map_err(|_| Error::with("invalid_provider_response", "model listing"))
    }
}

/// The shapes providers answer with: `data` (OpenAI, Anthropic, OpenRouter,
/// Bedrock Mantle) or `models` (the ChatGPT Codex backend, whose `hide` and
/// `none` entries its own picker leaves out too). Newest first where the
/// provider dates them, since OpenAI's order is arbitrary. Only models a text
/// turn can run.
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
    let mut entries: Vec<&Value> = entries.iter().collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry["created"].as_i64().unwrap_or(i64::MIN)));
    Ok(entries
        .into_iter()
        .filter(|entry| entry["visibility"].as_str().is_none_or(|v| v == "list"))
        .filter_map(|entry| {
            let id = entry["id"].as_str().or(entry["slug"].as_str())?;
            if !writes_text(entry, id) {
                return None;
            }
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

fn writes_text(entry: &Value, id: &str) -> bool {
    if let Some(output) = entry.pointer("/architecture/output_modalities") {
        return output
            .as_array()
            .is_some_and(|output| output.iter().any(|m| m == "text"));
    }
    let name = id.rsplit('/').next().unwrap_or(id).to_ascii_lowercase();
    !name
        .split(['-', '.', '_', ':'])
        .any(|word| NOT_TEXT.contains(&word))
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::{
        codec::Family,
        provider::{Provider, Transport},
    };
    use serde_json::json;

    #[test]
    fn each_listing_sits_where_its_provider_serves_it() {
        let transport = Transport::new(64, 1).unwrap();
        let url = |family, base: &str| {
            Provider::new(transport.clone(), family, base, None)
                .unwrap()
                .models_url()
                .to_string()
        };
        assert_eq!(
            url(Family::Responses, "https://api.openai.com/v1"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            url(Family::Anthropic, "https://api.anthropic.com/v1"),
            "https://api.anthropic.com/v1/models?limit=1000"
        );
        for family in [Family::Responses, Family::Anthropic] {
            let (route, own) = match family {
                Family::Responses => ("openai", "openai.gpt-6-sol"),
                Family::Anthropic => ("anthropic", "anthropic.claude-sonnet-5"),
            };
            let mantle = format!("https://bedrock-mantle.us-east-1.api.aws/{route}/v1");
            assert_eq!(
                url(family, &mantle).split('?').next().unwrap(),
                "https://bedrock-mantle.us-east-1.api.aws/v1/models"
            );
            // The one listing names both families; each route keeps its own.
            let provider = Provider::new(transport.clone(), family, &mantle, None).unwrap();
            let kept: Vec<_> = ["anthropic.claude-sonnet-5", "openai.gpt-6-sol"]
                .into_iter()
                .filter(|id| provider.serves(id))
                .collect();
            assert_eq!(kept, [own]);
        }
        assert!(
            Provider::new(
                transport.clone(),
                Family::Responses,
                "https://api.openai.com/v1",
                None
            )
            .unwrap()
            .serves("anthropic.claude-sonnet-5")
        );
        // The ChatGPT backend lists nothing without the client's version.
        let dir = std::env::temp_dir().join(format!("agent-models-login-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            r#"{"tokens":{"access_token":"synthetic-token","account_id":"synthetic-account"}}"#,
        )
        .unwrap();
        let login = std::sync::Arc::new(crate::provider::login::Login::open(&path, None).unwrap());
        let chatgpt = Provider::new(
            transport,
            Family::Responses,
            "https://chatgpt.com/backend-api/codex",
            None,
        )
        .unwrap()
        .with_login(login)
        .unwrap();
        assert_eq!(
            chatgpt.models_url().to_string(),
            format!(
                "https://chatgpt.com/backend-api/codex/models?client_version={}",
                super::CODEX_CLIENT_VERSION
            )
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

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
        // Models no text turn runs are left out, by the words in their ids.
        let openai = json!({"object":"list","data":[
            {"id":"gpt-5.6-luna","object":"model","created":1,"owned_by":"openai"},
            {"id":"gpt-6-luna","object":"model","created":2,"owned_by":"openai"},
            {"id":"text-embedding-3-large","created":3},{"id":"gpt-4o-mini-tts-2025-12-15","created":3},
            {"id":"whisper-1","created":3},{"id":"gpt-image-2","created":3},{"id":"dall-e-3","created":3},
            {"id":"gpt-realtime","created":3},{"id":"gpt-audio","created":3},{"id":"sora-2","created":3},
            {"id":"omni-moderation-latest","created":3},{"id":"gpt-4o-search-preview","created":3},
            {"id":"davinci-002","created":3},{"id":"gpt-3.5-turbo-instruct","created":3},
            {"id":"gpt-4o-transcribe","created":3}]});
        assert_eq!(
            parse(&openai).unwrap(),
            [json!({"id":"gpt-6-luna"}), json!({"id":"gpt-5.6-luna"})]
        );
        // A provider stating output modalities is taken at its word.
        let openrouter = json!({"data":[{"id":"vendor/model","name":"Vendor: Model",
            "context_length":200000,"top_provider":{"max_completion_tokens":64000}},
            {"id":"vendor/audio-model","architecture":{"output_modalities":["text","audio"]}},
            {"id":"vendor/painter","architecture":{"output_modalities":["image"]}}]});
        assert_eq!(
            parse(&openrouter).unwrap(),
            [
                json!({"id":"vendor/model","name":"Vendor: Model",
                "context_tokens":200000,"output_tokens":64000}),
                json!({"id":"vendor/audio-model"})
            ]
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
