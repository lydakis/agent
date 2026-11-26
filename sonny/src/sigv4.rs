//! MCP servers behind AWS IAM (`sigv4` and `region` in a server's config):
//! rmcp's own streamable-HTTP client over reqwest, each request signed with
//! AWS Signature Version 4 just before it goes, as AWS's MCP proxy
//! (mcp-proxy-for-aws) signs its own. The body's SHA-256 is signed, which
//! API Gateway's IAM auth requires and every service takes. Keys come from
//! the AWS chain through the runtime's own signer ([`agent_client::aws`]),
//! are held in memory only, and are re-resolved before they expire. Tools
//! and the events extension both travel this way, being requests to the
//! same server.
use agent_client::aws::{self, Aws};
use agent_client::config::Sigv4;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::streamable_http_client::{
    SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
    StreamableHttpResponseLimits,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

type Failed = StreamableHttpError<reqwest::Error>;

/// rmcp's reqwest client with a signer before it.
#[derive(Clone)]
pub struct Signed {
    http: reqwest::Client,
    aws: Arc<Aws>,
}

impl Signed {
    pub fn new(sigv4: &Sigv4) -> Result<Signed, String> {
        // rmcp's default client, but for redirects: a signature is for one
        // host and path, and its session token goes nowhere else.
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Signed {
            http,
            aws: signer(sigv4),
        })
    }

    /// Sign a request of `method` to `uri` whose body is `body`, adding the
    /// headers that carry the signature to `headers`.
    async fn sign(
        &self,
        method: &str,
        uri: &str,
        body: &[u8],
        headers: &mut HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), Failed> {
        let refused = |why: String| StreamableHttpError::Io(std::io::Error::other(why));
        let url = url::Url::parse(uri).map_err(|e| refused(format!("{uri}: {e}")))?;
        let keys = self
            .aws
            .current()
            .await
            .map_err(|e| refused(e.to_string()))?;
        let payload = aws::digest(body);
        for (name, value) in self
            .aws
            .sign(&keys, method, &url, SystemTime::now(), &payload)
        {
            let mut value = HeaderValue::from_str(&value)
                .map_err(|_| refused(format!("{name} is not a header value")))?;
            // Kept out of any debug print of the request.
            value.set_sensitive(matches!(name, "authorization" | "x-amz-security-token"));
            headers.insert(HeaderName::from_static(name), value);
        }
        Ok(())
    }
}

/// One signer for each service, region and kept AWS variables in this
/// process, so a server connected again, or several signed for the same,
/// reuse the keys the AWS CLI gave rather than ask it again. The AWS
/// variables in ~/.agent/keys/env (where setup keeps `AWS_PROFILE`) stand
/// over Sonny's environment, as they do for the daemon it starts and for
/// a bearer token's variable; kept anew there, they make a new signer.
fn signer(sigv4: &Sigv4) -> Arc<Aws> {
    type Signers = BTreeMap<(String, String, Vec<(String, String)>), Arc<Aws>>;
    static SIGNERS: Mutex<Signers> = Mutex::new(BTreeMap::new());
    let kept: Vec<(String, String)> = agent_client::env_file::path()
        .and_then(|file| agent_client::env_file::read(&file).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|(name, _)| name.starts_with("AWS_"))
        .collect();
    let mut signers = SIGNERS.lock().unwrap_or_else(|e| e.into_inner());
    let signer = signers
        .entry((sigv4.service.clone(), sigv4.region.clone(), kept))
        .or_insert_with_key(|(service, region, kept)| {
            Arc::new(Aws::new(region.clone(), service, None, kept.clone()))
        });
    Arc::clone(signer)
}

impl StreamableHttpClient for Signed {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, Failed> {
        self.post_message_with_response_limits(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            StreamableHttpResponseLimits::default(),
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, Failed> {
        let mut limits = StreamableHttpResponseLimits::default();
        limits.max_sse_event_size = max_sse_event_size;
        self.post_message_with_response_limits(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            limits,
        )
        .await
    }

    async fn post_message_with_response_limits(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        mut custom_headers: HashMap<HeaderName, HeaderValue>,
        limits: StreamableHttpResponseLimits,
    ) -> Result<StreamableHttpPostResponse, Failed> {
        // The bytes reqwest sends for it: its `json` is serde_json's to_vec.
        let body = serde_json::to_vec(&message)?;
        self.sign("POST", &uri, &body, &mut custom_headers).await?;
        self.http
            .post_message_with_response_limits(
                uri,
                message,
                session_id,
                auth_header,
                custom_headers,
                limits,
            )
            .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        mut custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), Failed> {
        self.sign("DELETE", &uri, b"", &mut custom_headers).await?;
        self.http
            .delete_session(uri, session_id, auth_header, custom_headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<futures_core::stream::BoxStream<'static, Result<sse_stream::Sse, SseError>>, Failed>
    {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            StreamableHttpResponseLimits::default().max_sse_event_size,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        mut custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<futures_core::stream::BoxStream<'static, Result<sse_stream::Sse, SseError>>, Failed>
    {
        self.sign("GET", &uri, b"", &mut custom_headers).await?;
        self.http
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                auth_header,
                custom_headers,
                max_sse_event_size,
            )
            .await
    }
}
