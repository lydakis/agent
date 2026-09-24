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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};

/// The server closes a connection at 60 minutes; one this old is not reused,
/// so a call never starts on a connection about to be cut.
const MAX_AGE: Duration = Duration::from_secs(55 * 60);
/// An idle bot's connection is closed after this, like an idle pooled one.
const IDLE: Duration = Duration::from_secs(60);
const CONNECT: Duration = Duration::from_secs(10);
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
    sessions: Mutex<Sessions>,
    tls: Arc<rustls::ClientConfig>,
}
struct Sessions {
    by_bot: HashMap<Box<str>, Session>,
    swept: Instant,
}

impl Sockets {
    pub fn new() -> Result<Self> {
        use rustls_platform_verifier::BuilderVerifierExt;
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .and_then(|builder| builder.with_platform_verifier())
            .map_err(|_| Error::new("http_client_init"))?
            .with_no_client_auth();
        // The upgrade is an HTTP/1.1 request; HTTP/2 would refuse it.
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self {
            sessions: Mutex::new(Sessions {
                by_bot: HashMap::new(),
                swept: Instant::now(),
            }),
            tls: Arc::new(tls),
        })
    }

    /// Open connections, for `stats`.
    pub fn open(&self) -> usize {
        self.sessions.lock().unwrap().by_bot.len()
    }

    /// Take a bot's connection for one call. Idle and aged connections are
    /// closed here, at most once a second, rather than by a timer task.
    pub(super) fn take(&self, bot: &str) -> Option<Session> {
        let mut sessions = self.sessions.lock().unwrap();
        let now = Instant::now();
        if now.duration_since(sessions.swept) >= Duration::from_secs(1) {
            sessions.swept = now;
            sessions.by_bot.retain(|_, session| {
                now.duration_since(session.used) < IDLE
                    && now.duration_since(session.opened) < MAX_AGE
            });
        }
        sessions
            .by_bot
            .remove(bot)
            .filter(|session| now.duration_since(session.opened) < MAX_AGE)
    }

    pub(super) fn put(&self, bot: &str, mut session: Session) {
        session.used = Instant::now();
        self.sessions
            .lock()
            .unwrap()
            .by_bot
            .insert(bot.into(), session);
    }

    /// The caller recorded the last response's items as these window ids, so
    /// the next request that extends them can send only what follows.
    pub(super) fn recorded(&self, bot: &str, ids: &[i64]) {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(last) = sessions
            .by_bot
            .get_mut(bot)
            .and_then(|session| session.last.as_mut())
            .filter(|last| !last.armed)
        {
            last.baseline.extend_from_slice(ids);
            last.armed = true;
        }
    }

    pub(super) async fn connect(
        &self,
        url: &reqwest::Url,
        headers: &[(&'static str, &str)],
    ) -> Result<(Session, HeaderMap)> {
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
            tokio_tungstenite::client_async_with_config(request, io, None),
        )
        .await
        .map_err(|_| Error::new("provider_connection_timeout"))?
        .map_err(|error| match error {
            tungstenite::Error::Http(response) => Error {
                code: format!("provider_http_{}", response.status().as_u16()),
                detail: None,
            },
            _ => Error::new("provider_connection_handshake"),
        })?;
        let now = Instant::now();
        Ok((
            Session {
                socket,
                opened: now,
                used: now,
                last: None,
            },
            response.headers().clone(),
        ))
    }
}

impl Session {
    /// Continue from the previous response when this request has the same
    /// fields and context head and its window extends what the server saw.
    pub(super) fn plan(&mut self, key: u64, ids: Option<&[i64]>) -> Plan {
        plan(self.last.take(), key, ids)
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
    /// Only content renews the stall bound; pings do not.
    pub(super) async fn exchange<F, Fut>(
        &mut self,
        text: String,
        parser: &mut responses::State,
        delta: &mut F,
        stall: Duration,
    ) -> std::result::Result<(), Failure>
    where
        F: FnMut(Delta) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        self.socket
            .send(Message::text(text))
            .await
            .map_err(|_| Failure::transport("provider_stream_failed"))?;
        let deadline = tokio::time::sleep(stall);
        tokio::pin!(deadline);
        let mut total = 0usize;
        while !parser.done() {
            let message = tokio::select! {
                message = self.socket.next() => message,
                () = &mut deadline => return Err(Failure::transport("provider_stream_stalled")),
            };
            let text = match message {
                Some(Ok(Message::Text(text))) => text,
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                Some(Ok(Message::Binary(_))) => {
                    return Err(Failure::transport("provider_unexpected_binary"));
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                    return Err(Failure::transport("provider_stream_failed"));
                }
            };
            total += text.len();
            if total > 16 * 1024 * 1024 {
                return Err(Failure::transport("provider_response_limit"));
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

/// Why a call on a socket failed, and whether the connection is still usable.
pub(super) struct Failure {
    pub error: Error,
    /// The connection is broken or closing; drop it.
    pub dead: bool,
    /// Status and headers an error event carried, for pacing.
    pub status: Option<u16>,
    pub headers: Option<Box<HeaderMap>>,
}
impl Failure {
    fn transport(code: &'static str) -> Self {
        Self {
            error: Error::new(code),
            dead: true,
            status: None,
            headers: None,
        }
    }
    /// The caller's own failure, such as a closed event consumer. The
    /// response is still streaming, so the connection cannot be reused.
    fn call(error: Error) -> Self {
        Self {
            error,
            dead: true,
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

fn plan(last: Option<Last>, key: u64, ids: Option<&[i64]>) -> Plan {
    match (last, ids) {
        (Some(last), Some(ids))
            if last.armed && last.key == key && ids.starts_with(&last.baseline) =>
        {
            Plan {
                previous: Some(last.response),
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
        let continued = plan(last(true), 7, Some(&[1, 2, 3, 4]));
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
            let full = plan(last, key, ids);
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
}
