//! Responses over WebSocket. Each bot keeps one connection across its calls,
//! and the server keeps that connection's latest response in memory, so a
//! call that extends the bot's previous request sends only the new items with
//! `previous_response_id`. The store stays the only history: the cached
//! response is an optimization the runtime may lose at any time, and every
//! call that is not an exact extension sends the full input.
use super::{Delta, Frame, responses};
use crate::{Error, Result};
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{
    self, Message, client::IntoClientRequest, protocol::WebSocketConfig,
};

/// The server closes a connection at 60 minutes; one this old is not reused,
/// so a call never starts on a connection about to be cut.
const MAX_AGE: Duration = Duration::from_secs(55 * 60);
/// An idle bot's connection is closed after this, like an idle pooled one.
const IDLE: Duration = Duration::from_secs(60);
/// How often idle and aged connections are looked for.
const SWEEP: Duration = Duration::from_secs(5);
const CONNECT: Duration = Duration::from_secs(10);
/// Bytes one response may deliver, as on the HTTP path. Messages and frames
/// are bounded by it too, so one event cannot allocate past it.
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
/// Codex sends this with every socket; OpenAI's guide names no header.
const BETA: &str = "responses_websockets=2026-02-06";

trait Io: AsyncRead + AsyncWrite + Send {}
impl<T: AsyncRead + AsyncWrite + Send> Io for T {}
type Socket = WebSocketStream<Pin<Box<dyn Io>>>;

pub(super) struct Session {
    socket: Socket,
    opened: Instant,
    used: Instant,
    last: Option<Last>,
    /// The provider's count of open connections, held or checked out.
    live: Arc<AtomicUsize>,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::Relaxed);
    }
}

/// What the server holds for this connection: the previous response, and the
/// request it answered.
struct Last {
    response: String,
    /// Digest of every request field but the input, and of the context bytes
    /// ahead of the window items.
    key: u64,
    /// Window ids the server has seen, input and output. Only armed once the
    /// caller has recorded the response's items and named their ids.
    baseline: Vec<i64>,
    armed: bool,
}

/// A request's position in its bot's history; see [`super::Chain`].
pub(super) struct Plan {
    pub previous: Option<String>,
    /// Window ids already on the server; the input is the ids after them.
    pub skip: usize,
}

pub struct Sockets {
    by_bot: Mutex<HashMap<Box<str>, Session>>,
    live: Arc<AtomicUsize>,
    tls: Arc<rustls::ClientConfig>,
}

impl Sockets {
    /// Idle and aged connections are closed by a task that holds only a weak
    /// reference, so it ends with the provider.
    pub fn new() -> Result<Arc<Self>> {
        use rustls_platform_verifier::BuilderVerifierExt;
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .and_then(|builder| builder.with_platform_verifier())
            .map_err(|_| Error::new("http_client_init"))?
            .with_no_client_auth();
        // The upgrade is an HTTP/1.1 request; HTTP/2 would refuse it.
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let sockets = Arc::new(Self {
            by_bot: Mutex::new(HashMap::new()),
            live: Arc::new(AtomicUsize::new(0)),
            tls: Arc::new(tls),
        });
        let weak = Arc::downgrade(&sockets);
        tokio::runtime::Handle::try_current()
            .map_err(|_| Error::new("http_client_init"))?
            .spawn(async move {
                let mut tick = tokio::time::interval(SWEEP);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    let Some(sockets) = weak.upgrade() else { break };
                    sockets.sweep(Instant::now());
                }
            });
        Ok(sockets)
    }

    /// Open connections, idle or in a call, for `stats`.
    pub fn open(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    /// Close connections idle past the bound or near the server's age limit.
    /// They are dropped after the lock is released.
    fn sweep(&self, now: Instant) {
        let mut closed = Vec::new();
        {
            let mut by_bot = self.by_bot.lock().unwrap();
            let stale: Vec<Box<str>> = by_bot
                .iter()
                .filter(|(_, session)| {
                    now.duration_since(session.used) >= IDLE
                        || now.duration_since(session.opened) >= MAX_AGE
                })
                .map(|(bot, _)| bot.clone())
                .collect();
            for bot in stale {
                closed.extend(by_bot.remove(&bot));
            }
        }
        drop(closed);
    }

    /// Close a deleted bot's connection now rather than at the next sweep.
    pub(super) fn forget(&self, bot: &str) {
        let closed = self.by_bot.lock().unwrap().remove(bot);
        drop(closed);
    }

    /// Take a bot's connection for one call, unless it is near the age limit.
    pub(super) fn take(&self, bot: &str) -> Option<Session> {
        let session = self.by_bot.lock().unwrap().remove(bot)?;
        (session.opened.elapsed() < MAX_AGE).then_some(session)
    }

    pub(super) fn put(&self, bot: &str, mut session: Session) {
        session.used = Instant::now();
        self.by_bot.lock().unwrap().insert(bot.into(), session);
    }

    /// The caller recorded the last response's items as these window ids, so
    /// the next request that extends them can send only what follows.
    pub(super) fn recorded(&self, bot: &str, ids: &[i64]) {
        let mut by_bot = self.by_bot.lock().unwrap();
        if let Some(last) = by_bot
            .get_mut(bot)
            .and_then(|session| session.last.as_mut())
            .filter(|last| !last.armed)
        {
            last.baseline.extend_from_slice(ids);
            last.armed = true;
        }
    }

    /// Open a connection. A refused upgrade carries its status and headers,
    /// so a 429's `retry-after` paces the pool as on the HTTP path.
    pub(super) async fn connect(
        &self,
        url: &reqwest::Url,
        headers: &[(&'static str, &str)],
    ) -> std::result::Result<(Session, HeaderMap), Failure> {
        self.open_socket(url, headers)
            .await
            .map_err(|error| match error {
                Opening::Refused(failure) => failure,
                Opening::Failed(error) => Failure::transport(error),
            })
    }

    async fn open_socket(
        &self,
        url: &reqwest::Url,
        headers: &[(&'static str, &str)],
    ) -> std::result::Result<(Session, HeaderMap), Opening> {
        let secure = url.scheme() == "https";
        let host = url.host_str().ok_or(Error::new("invalid_provider_url"))?;
        let port = url
            .port_or_known_default()
            .ok_or(Error::new("invalid_provider_url"))?;
        let tcp = tokio::time::timeout(CONNECT, tokio::net::TcpStream::connect((host, port)))
            .await
            .map_err(|_| Error::new("provider_connection_timeout"))?
            .map_err(|error| Error::with("provider_connection_failed", error.kind().to_string()))?;
        let _ = tcp.set_nodelay(true);
        let io: Pin<Box<dyn Io>> = if secure {
            let name = rustls::pki_types::ServerName::try_from(host.to_owned())
                .map_err(|_| Error::new("invalid_provider_url"))?;
            let tls = tokio::time::timeout(
                CONNECT,
                tokio_rustls::TlsConnector::from(self.tls.clone()).connect(name, tcp),
            )
            .await
            .map_err(|_| Error::new("provider_connection_timeout"))?
            .map_err(|error| Error::with("provider_connection_tls", error.kind().to_string()))?;
            Box::pin(tls)
        } else {
            Box::pin(tcp)
        };
        let mut target = url.clone();
        let _ = target.set_scheme(if secure { "wss" } else { "ws" });
        let mut request = target
            .as_str()
            .into_client_request()
            .map_err(|_| Error::new("invalid_provider_url"))?;
        for (name, value) in headers.iter().copied().chain([("openai-beta", BETA)]) {
            request.headers_mut().insert(
                HeaderName::from_static(name),
                HeaderValue::from_str(value).map_err(|_| Error::new("invalid_provider_header"))?,
            );
        }
        let (socket, response) = tokio::time::timeout(
            CONNECT,
            tokio_tungstenite::client_async_with_config(
                request,
                io,
                Some(
                    WebSocketConfig::default()
                        .max_message_size(Some(MAX_RESPONSE))
                        .max_frame_size(Some(MAX_RESPONSE)),
                ),
            ),
        )
        .await
        .map_err(|_| Error::new("provider_connection_timeout"))?
        .map_err(|error| match error {
            tungstenite::Error::Http(response) => {
                let status = response.status().as_u16();
                let quota = status == 429
                    && response.body().as_deref().is_some_and(|body| {
                        String::from_utf8_lossy(body).contains("insufficient_quota")
                    });
                Opening::Refused(Failure {
                    error: Error {
                        code: if quota {
                            "provider_quota_exhausted".into()
                        } else {
                            format!("provider_http_{status}")
                        },
                        detail: None,
                    },
                    dead: true,
                    refused: true,
                    status: (!quota).then_some(status),
                    headers: Some(Box::new(response.headers().clone())),
                })
            }
            _ => Opening::Failed(Error::new("provider_connection_handshake")),
        })?;
        let now = Instant::now();
        self.live.fetch_add(1, Ordering::Relaxed);
        Ok((
            Session {
                socket,
                opened: now,
                used: now,
                last: None,
                live: self.live.clone(),
            },
            response.headers().clone(),
        ))
    }
}

impl Session {
    /// Continue from the previous response when this request has the same
    /// fields and context head and its window extends what the server saw.
    pub(super) fn plan(&self, key: u64, ids: Option<&[i64]>) -> Plan {
        plan(self.last.as_ref(), key, ids)
    }

    /// A failed call leaves nothing to continue from, unless the provider
    /// refused it outright for another reason than a lost response: no
    /// response was made, so the connection still holds the previous one and
    /// a retry can continue from it.
    pub(super) fn failed(&mut self, failure: &Failure) {
        if !failure.refused
            || failure.dead
            || failure.error.code == "provider_previous_response_not_found"
        {
            self.last = None;
        }
    }

    /// Remember a completed response. A request without window ids (a
    /// summary over a span) leaves nothing to continue from.
    pub(super) fn completed(&mut self, response: Option<&str>, key: u64, ids: Option<&[i64]>) {
        self.last = response.zip(ids).map(|(response, ids)| Last {
            response: response.to_owned(),
            key,
            baseline: ids.to_vec(),
            armed: false,
        });
    }

    /// Send one `response.create` and read its events until the terminal one.
    /// The stall bound covers the write, since a peer that stops reading
    /// blocks it, and only content renews it; pings do not. The startup
    /// permit is released when the first event arrives.
    pub(super) async fn exchange<F, Fut>(
        &mut self,
        text: String,
        parser: &mut responses::State,
        delta: &mut F,
        stall: Duration,
        admission: &mut Option<tokio::sync::SemaphorePermit<'_>>,
    ) -> std::result::Result<(), Failure>
    where
        F: FnMut(Delta) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        match tokio::time::timeout(stall, self.socket.send(Message::text(text))).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(Failure::unsent(Error::new("provider_stream_failed"))),
            // Part of the request may have reached the provider.
            Err(_) => return Err(Failure::transport(Error::new("provider_stream_stalled"))),
        }
        let deadline = tokio::time::sleep(stall);
        tokio::pin!(deadline);
        let mut total = 0usize;
        while !parser.done() {
            let message = tokio::select! {
                message = self.socket.next() => message,
                () = &mut deadline => return Err(Failure::transport(Error::new("provider_stream_stalled"))),
            };
            let text = match message {
                // Startup is bounded until the provider answers, as on HTTP;
                // a control frame is the connection's, not an answer.
                Some(Ok(Message::Text(text))) => {
                    admission.take();
                    text
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                Some(Ok(Message::Binary(_))) => {
                    return Err(Failure::transport(Error::new("provider_unexpected_binary")));
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                    return Err(Failure::transport(Error::new("provider_stream_failed")));
                }
            };
            total += text.len();
            if total > MAX_RESPONSE {
                return Err(Failure::transport(Error::new("provider_response_limit")));
            }
            match parser.frame(text.as_bytes()) {
                Ok(Frame::Delta(part)) => delta(part).await.map_err(Failure::call)?,
                Ok(Frame::Quiet | Frame::Keepalive) => {}
                Err(error) => return Err(Failure::event(error, text.as_str())),
            }
            deadline.as_mut().reset(tokio::time::Instant::now() + stall);
        }
        Ok(())
    }
}

enum Opening {
    Refused(Failure),
    Failed(Error),
}
impl From<Error> for Opening {
    fn from(error: Error) -> Self {
        Opening::Failed(error)
    }
}

/// Why a call on a socket failed, and whether the connection is still usable.
pub(super) struct Failure {
    pub error: Error,
    /// The connection is broken or closing; drop it.
    pub dead: bool,
    /// The provider refused the request before any inference.
    pub refused: bool,
    /// Status and headers an error event carried, for pacing.
    pub status: Option<u16>,
    pub headers: Option<Box<HeaderMap>>,
}
impl Failure {
    fn transport(error: Error) -> Self {
        Self {
            error,
            dead: true,
            refused: false,
            status: None,
            headers: None,
        }
    }
    /// The request could not be written, as when the provider had closed an
    /// idle connection: no inference ran, so it costs nothing.
    fn unsent(error: Error) -> Self {
        Self {
            refused: true,
            ..Self::transport(error)
        }
    }
    /// The caller's own failure, such as a closed event consumer. The
    /// response is still streaming, so the connection cannot be reused.
    fn call(error: Error) -> Self {
        Self {
            error,
            dead: true,
            refused: false,
            status: None,
            headers: None,
        }
    }
    /// A failed event. Only a terminal event (`error`, `response.failed`,
    /// `response.incomplete`) leaves the connection idle and reusable; any
    /// other failure stops reading mid-response. The socket wraps HTTP-level
    /// refusals in an `error` event with a status and headers; two codes are
    /// about the connection, not the request, and get their own error codes.
    fn event(error: Error, text: &str) -> Self {
        let value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        let mut failure = Self::call(error);
        failure.dead = !matches!(
            value["type"].as_str(),
            Some("error" | "response.failed" | "response.incomplete")
        );
        if value["type"] != "error" {
            return failure;
        }
        // An `error` event ends the request before any output.
        failure.refused = true;
        let quota = [&value["error"]["code"], &value["error"]["type"]]
            .iter()
            .any(|field| field.as_str() == Some("insufficient_quota"));
        if quota {
            failure.error.code = "provider_quota_exhausted".into();
            return failure;
        }
        match value["error"]["code"].as_str() {
            Some("previous_response_not_found") => {
                failure.error = Error::new("provider_previous_response_not_found");
                return failure;
            }
            Some("websocket_connection_limit_reached") => {
                failure.error = Error::new("provider_socket_expired");
                failure.dead = true;
                return failure;
            }
            _ => {}
        }
        failure.status = value["status"]
            .as_u64()
            .or_else(|| value["status_code"].as_u64())
            .and_then(|status| u16::try_from(status).ok());
        failure.headers = value["headers"].as_object().map(|headers| {
            Box::new(
                headers
                    .iter()
                    .filter_map(|(name, value)| {
                        let value = match value {
                            Value::String(value) => value.clone(),
                            Value::Number(value) => value.to_string(),
                            _ => return None,
                        };
                        Some((
                            HeaderName::from_bytes(name.as_bytes()).ok()?,
                            HeaderValue::from_str(&value).ok()?,
                        ))
                    })
                    .collect(),
            )
        });
        // A rate limit keeps the parser's classification; any other refusal
        // with a status is that HTTP status, as on the HTTP path.
        if let Some(status) = failure.status.filter(|status| !(200..300).contains(status))
            && failure.error.code != "provider_rate_limited"
        {
            failure.error.code = format!("provider_http_{status}");
        }
        failure
    }
}

fn plan(last: Option<&Last>, key: u64, ids: Option<&[i64]>) -> Plan {
    match (last, ids) {
        (Some(last), Some(ids))
            if last.armed && last.key == key && ids.starts_with(&last.baseline) =>
        {
            Plan {
                previous: Some(last.response.clone()),
                skip: last.baseline.len(),
            }
        }
        _ => Plan {
            previous: None,
            skip: 0,
        },
    }
}

/// Digest of a request's non-input fields and the context ahead of its window.
pub(super) fn key(fields: &[u8], head: &[u8]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    fields.hash(&mut hasher);
    head.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_exact_extension_of_a_recorded_response_continues() {
        let last = |armed| {
            Some(Last {
                response: "resp_1".into(),
                key: 7,
                baseline: vec![1, 2, 3],
                armed,
            })
        };
        let continued = plan(last(true).as_ref(), 7, Some(&[1, 2, 3, 4]));
        assert_eq!(
            (continued.previous.as_deref(), continued.skip),
            (Some("resp_1"), 3)
        );
        // Not yet recorded, other fields or context head, a window that
        // dropped or rewrote an item, a request with no window, nothing held.
        for (last, key, ids) in [
            (last(false), 7, Some(&[1, 2, 3, 4][..])),
            (last(true), 8, Some(&[1, 2, 3, 4][..])),
            (last(true), 7, Some(&[2, 3, 4][..])),
            (last(true), 7, Some(&[1, 2, 9, 4][..])),
            (last(true), 7, None),
            (None, 7, Some(&[1, 2, 3, 4][..])),
        ] {
            let full = plan(last.as_ref(), key, ids);
            assert_eq!((full.previous, full.skip), (None, 0));
        }
    }

    #[test]
    fn connection_error_events_get_their_own_codes() {
        let missing = Failure::event(
            Error::new("provider_incomplete"),
            r#"{"type":"error","status":400,"error":{"code":"previous_response_not_found","message":"Previous response with id 'resp_1' not found."}}"#,
        );
        assert_eq!(missing.error.code, "provider_previous_response_not_found");
        assert!(!missing.dead);
        let expired = Failure::event(
            Error::new("provider_incomplete"),
            r#"{"type":"error","status":400,"error":{"code":"websocket_connection_limit_reached"}}"#,
        );
        assert_eq!(expired.error.code, "provider_socket_expired");
        assert!(expired.dead);
        let quota = Failure::event(
            Error::new("provider_incomplete"),
            r#"{"type":"error","status":429,"error":{"code":"insufficient_quota"}}"#,
        );
        assert_eq!(quota.error.code, "provider_quota_exhausted");
        assert!(quota.status.is_none());
        // A failure mid-response leaves events in flight on the connection.
        let midway = Failure::event(
            Error::new("output_limit"),
            r#"{"type":"response.output_text.delta","delta":"x"}"#,
        );
        assert!(midway.dead);
        let failed = Failure::event(
            Error::new("provider_incomplete"),
            r#"{"type":"response.failed","response":{"status":"failed"}}"#,
        );
        assert!(!failed.dead);
        let limited = Failure::event(
            Error::new("provider_incomplete"),
            r#"{"type":"error","status":429,"error":{"code":"other"},"headers":{"retry-after":"2","x-ratelimit-limit-requests":10}}"#,
        );
        assert_eq!(limited.error.code, "provider_http_429");
        let headers = limited.headers.unwrap();
        assert_eq!(headers["retry-after"], "2");
        assert_eq!(headers["x-ratelimit-limit-requests"], "10");
    }

    #[test]
    fn an_overloaded_status_closes_the_pool_as_on_http() {
        let pace = super::super::pace::Pace::default();
        let overloaded = Failure::event(
            Error::new("provider_incomplete"),
            r#"{"type":"error","status":529,"error":{"code":"overloaded"},"headers":{"retry-after":"30"}}"#,
        );
        assert_eq!(overloaded.error.code, "provider_http_529");
        super::super::limit(&pace, &overloaded);
        assert!(pace.snapshot().4);
    }
}
