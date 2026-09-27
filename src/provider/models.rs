//! The models a provider lists for the credentials the daemon holds. Asked
//! only when a client wants them, never on the turn path, and kept for five
//! minutes, as Codex and OpenCode keep their catalogs. The daemon does not
//! consult the list to run anything: a turn runs whatever model it names.
use super::{Provider, aws, connection_error, error_body, sanitize_error};
use crate::{Error, Result, codec::Family, fail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{Arc, atomic::Ordering};
use std::time::{Duration, Instant};

const KEEP: Duration = Duration::from_secs(300);
/// The Codex release whose model list a ChatGPT login is offered: Codex
/// sends its own version as `client_version` (openai/codex 9db8162, read
/// 2026-09-27), and the backend answers 400 without it. 0.157.1 is the
/// release the rest of this repository measures against.
const CODEX_CLIENT_VERSION: &str = "0.157.1";
const DEADLINE: Duration = Duration::from_secs(10);
/// Anthropic's pages hold 1,000 models; nobody lists fifty thousand.
const PAGES: usize = 50;
/// OpenRouter's catalog is the largest known, a few hundred KiB.
const LIMIT: usize = 8 * 1024 * 1024;
/// SHA-256 of an empty body, for a signer that signs the payload.
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Listings and refusals kept across all providers, about two replies'
/// worth; past it an answer is given but not kept.
const KEPT: usize = 2 * crate::output::MAX_EVENT;

/// The last answer, a refusal included, so a client asking again within
/// `KEEP` waits on neither the network nor a provider that is down, with
/// the bytes it holds of the transport's shared `KEPT`. A listing is kept
/// as its JSON text, so what it holds is what it counts.
#[derive(Default)]
pub struct Listing(tokio::sync::Mutex<Option<(Instant, Listed, usize)>>);

/// A JSON array of `{id, name?, context_tokens?, output_tokens?}`.
pub type Listed = Result<Arc<str>>;

impl Provider {
    /// `{id, name?, context_tokens?, output_tokens?}` per model, ids without
    /// the provider prefix. Callers asking at once share one request.
    /// Refused credentials are not kept: a login made meanwhile (`aws sso
    /// login`, Codex signing in) is used on the next ask.
    pub async fn models(&self) -> Listed {
        let mut kept = self.listing.0.lock().await;
        if let Some((at, listed, _)) = kept.as_ref()
            && at.elapsed() < KEEP
        {
            return listed.clone();
        }
        if let Some((_, _, held)) = kept.take() {
            self.transport.listed.fetch_sub(held, Ordering::Relaxed);
        }
        let listed: Listed = tokio::time::timeout(DEADLINE, self.list_models())
            .await
            .unwrap_or_else(|_| fail("provider_connection_timeout"))
            .and_then(|models| {
                // Answered only if one reply could carry it.
                let text = serde_json::to_string(&models)?;
                match text.len() <= crate::output::MAX_EVENT {
                    true => Ok(text.into()),
                    false => Err(Error::with(
                        "provider_models_limit",
                        format!("{} models, {} bytes listed", models.len(), text.len()),
                    )),
                }
            });
        if listed.as_ref().is_err_and(|error| {
            ["provider_aws_credentials_", "provider_login_"]
                .iter()
                .any(|refused| error.code.starts_with(refused))
        }) {
            return listed;
        }
        // A refusal holds its code and detail; it counts like a listing.
        let held = match &listed {
            Ok(text) => text.len(),
            Err(error) => 64 + error.code.len() + error.detail.as_ref().map_or(0, String::len),
        };
        let room =
            self.transport
                .listed
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |kept| {
                    (kept + held <= KEPT).then_some(kept + held)
                });
        if room.is_ok() {
            *kept = Some((Instant::now(), listed.clone(), held));
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
            // The Codex backend lists models for a Codex version and refuses
            // a request without one.
            url.query_pairs_mut()
                .append_pair("client_version", CODEX_CLIENT_VERSION);
        }
        url
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
                // Mantle lists every family at one address; offer only those
                // this binding's wire format runs.
                if self.url.host_str() == Some("api.openai.com") {
                    models.retain(|model| model["id"].as_str().is_some_and(openai_text));
                }
                if matches!(aws::endpoint(&self.url), Some((_, "bedrock-mantle"))) {
                    models.retain(|model| {
                        model["id"]
                            .as_str()
                            .is_some_and(|id| mantle_runs(self.family, id))
                    });
                }
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

/// OpenAI's listing names every model on the account, speech, embedding,
/// image and moderation ones included, with nothing to tell them apart but
/// the name. Leave out those families, which a Responses turn cannot run.
/// The families are read from OpenAI's model names (inferred from the
/// listing seen 2026-09-27, not from a published capability field).
fn openai_text(id: &str) -> bool {
    const NOT_TEXT: [&str; 13] = [
        "embedding",
        "tts",
        "whisper",
        "transcribe",
        "realtime",
        "audio",
        "dall-e",
        "image",
        "moderation",
        "davinci",
        "babbage",
        "sora",
        "search",
    ];
    // A fine-tuned model is named `ft:BASE:org:suffix:id`; only BASE says
    // what it is.
    let base = id.strip_prefix("ft:").unwrap_or(id);
    let base = base.split(':').next().unwrap_or(base);
    !NOT_TEXT.iter().any(|family| base.contains(family))
}

/// Whether a Mantle model id is one of the family its binding speaks:
/// Anthropic's models (`anthropic.`, perhaps under a routing prefix such as
/// `global.`) on the Messages route, everything else on Responses.
fn mantle_runs(family: Family, id: &str) -> bool {
    id.split('.').any(|part| part == "anthropic") == (family == Family::Anthropic)
}

/// The shapes providers answer with: `data` (OpenAI, Anthropic, OpenRouter,
/// Bedrock Mantle) or `models` (the ChatGPT Codex backend, whose `hide` and
/// `none` entries its own picker leaves out too). Newest first where the
/// provider dates them, since OpenAI's order is arbitrary.
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
    use crate::{
        codec::Family,
        provider::{Provider, Transport},
    };
    use serde_json::json;

    #[tokio::test]
    async fn a_kept_refusal_counts_against_the_shared_budget() {
        use std::sync::atomic::Ordering;
        let transport = Transport::new(64, 1).unwrap();
        let provider = Provider::new(
            transport.clone(),
            Family::Responses,
            "http://127.0.0.1:1/v1",
            None,
        )
        .unwrap();
        let refused = provider.models().await.unwrap_err();
        let held = transport.listed.load(Ordering::Relaxed);
        assert!(held >= refused.code.len(), "{held}");
        // Asked again, the kept refusal answers and is not counted twice.
        assert_eq!(provider.models().await.unwrap_err(), refused);
        assert_eq!(transport.listed.load(Ordering::Relaxed), held);
    }

    #[tokio::test]
    async fn a_refused_login_is_asked_again_not_kept() {
        use crate::provider::aws::{Aws, Keys};
        use std::sync::{Arc, atomic::Ordering};
        let transport = Transport::new(64, 1).unwrap();
        let mut keys = Keys::new("A".into(), "S".into(), None);
        keys.expires = Some(std::time::UNIX_EPOCH);
        let aws = Arc::new(Aws::fixed("us-east-1", "bedrock-mantle", keys));
        let provider = Provider::new(
            transport.clone(),
            Family::Responses,
            "https://bedrock-mantle.us-east-1.api.aws/v1/responses",
            None,
        )
        .unwrap()
        .with_aws(aws)
        .unwrap();
        let refused = provider.models().await.unwrap_err();
        assert_eq!(refused.code, "provider_aws_credentials_expired");
        assert!(provider.listing.0.lock().await.is_none());
        assert_eq!(transport.listed.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn openai_offers_only_models_a_turn_can_run() {
        use super::openai_text;
        for id in [
            "gpt-6-luna",
            "gpt-6-sol",
            "o5-mini",
            "gpt-5.6-luna",
            "codex-mini-latest",
            "ft:gpt-4.1:org:customer-search:abc123",
        ] {
            assert!(openai_text(id), "{id}");
        }
        for id in [
            "text-embedding-3-large",
            "tts-1-hd",
            "whisper-1",
            "gpt-4o-transcribe",
            "gpt-realtime",
            "gpt-audio",
            "dall-e-3",
            "gpt-image-1",
            "omni-moderation-latest",
            "davinci-002",
            "babbage-002",
            "sora-2",
            "gpt-4o-search-preview",
            "ft:babbage-002:org:chat:abc123",
        ] {
            assert!(!openai_text(id), "{id}");
        }
    }

    #[test]
    fn mantle_offers_each_binding_only_its_own_family() {
        use super::mantle_runs;
        assert!(mantle_runs(Family::Anthropic, "anthropic.claude-sonnet-5"));
        assert!(mantle_runs(
            Family::Anthropic,
            "global.anthropic.claude-sonnet-5"
        ));
        assert!(!mantle_runs(Family::Anthropic, "openai.gpt-6-luna"));
        assert!(mantle_runs(Family::Responses, "openai.gpt-6-luna"));
        assert!(!mantle_runs(Family::Responses, "anthropic.claude-sonnet-5"));
    }

    #[test]
    fn each_listing_sits_where_its_provider_serves_it() {
        let transport = Transport::new(64, 1).unwrap();
        let dir = std::env::temp_dir().join(format!("agent-models-login-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let auth = dir.join("auth.json");
        std::fs::write(
            &auth,
            r#"{"tokens":{"access_token":"synthetic-token","account_id":"synthetic-account"}}"#,
        )
        .unwrap();
        let login = std::sync::Arc::new(crate::provider::login::Login::open(&auth, None).unwrap());
        let chatgpt = Provider::new(
            transport.clone(),
            Family::Responses,
            "https://chatgpt.com/backend-api/codex",
            None,
        )
        .unwrap()
        .with_login(login)
        .unwrap();
        assert_eq!(
            chatgpt.models_url().as_str(),
            "https://chatgpt.com/backend-api/codex/models?client_version=0.157.1"
        );
        std::fs::remove_dir_all(&dir).unwrap();
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
            let route = match family {
                Family::Responses => "openai",
                Family::Anthropic => "anthropic",
            };
            let mantle = format!("https://bedrock-mantle.us-east-1.api.aws/{route}/v1");
            assert_eq!(
                url(family, &mantle).split('?').next().unwrap(),
                "https://bedrock-mantle.us-east-1.api.aws/v1/models"
            );
        }
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
        let openai = json!({"object":"list","data":[
            {"id":"gpt-5.6-luna","object":"model","created":1,"owned_by":"openai"},
            {"id":"gpt-6-luna","object":"model","created":2,"owned_by":"openai"}]});
        assert_eq!(
            parse(&openai).unwrap(),
            [json!({"id":"gpt-6-luna"}), json!({"id":"gpt-5.6-luna"})]
        );
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
