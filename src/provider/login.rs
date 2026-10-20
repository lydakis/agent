//! A ChatGPT plan login: the token set `agent login chatgpt` saved through
//! Sign in with ChatGPT (developers.openai.com/siwc, read 2026-10-20). The
//! daemon owns it from then on. Access tokens last an hour and refresh tokens
//! are single-use, so the daemon refreshes near expiry, one refresh at a time
//! across processes, and writes the rotated pair back before using it. A
//! token set the file holds that differs from the one in memory (a new
//! sign-in, another daemon's refresh) is taken as it is.
use crate::{Error, Result, tools::Credentials};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// The environment names the tokens are redacted under; no variable carries them.
pub const TOKEN_NAME: &str = "AGENT_CHATGPT_TOKEN";
const REFRESH_NAME: &str = "AGENT_CHATGPT_REFRESH";
const ID_NAME: &str = "AGENT_CHATGPT_ID_TOKEN";
pub const ISSUER: &str = "https://auth.openai.com";
pub const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
/// The audience every grant and refresh names, and the inference endpoint.
pub const RESOURCE: &str = "https://api.openai.com/v1";
/// The scope that lets a token spend the plan; an ID token alone does not.
pub const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
/// Refresh this long before the access token expires, so a request that
/// starts just before expiry does not carry a token that dies in flight.
const MARGIN: Duration = Duration::from_secs(120);
/// A token refreshed this recently and still refused is not refreshed
/// again: the refusal is about the grant, not the token's age.
const FRESH: Duration = Duration::from_secs(60);
const DEADLINE: Duration = Duration::from_secs(30);
const LIMIT: usize = 64 * 1024;
const HINT: &str = "run agent login chatgpt to sign in again";

/// What `agent login chatgpt` writes and each refresh rewrites.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    /// The client OpenAI issued at registration (`oaiapp_…`), reused for
    /// every refresh and later sign-in.
    pub client_id: String,
    /// The account's verified `sub`; a later sign-in must return the same.
    pub subject: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Kept for `id_token_hint` on the next sign-in.
    pub id_token: String,
    /// Seconds since the epoch.
    pub expires_at: u64,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl std::fmt::Debug for Saved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Saved")
            .field("client_id", &self.client_id)
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl Saved {
    fn expires(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.expires_at)
    }
    fn due(&self) -> bool {
        self.expires() <= SystemTime::now() + MARGIN
    }
}

/// `AGENT_CHATGPT_LOGIN`, else `~/.agent/chatgpt.json`: where `agent login
/// chatgpt` writes and the daemon reads.
pub fn default_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("AGENT_CHATGPT_LOGIN").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".agent").join("chatgpt.json"))
        .ok_or(Error::with("provider_login_unavailable", "HOME is not set"))
}

pub fn read(path: &Path) -> Result<Saved> {
    let unavailable = || {
        Error::with(
            "provider_login_unavailable",
            format!("{}; run agent login chatgpt", path.display()),
        )
    };
    let saved: Saved = serde_json::from_slice(&std::fs::read(path).map_err(|_| unavailable())?)
        .map_err(|_| unavailable())?;
    if [&saved.client_id, &saved.access_token, &saved.refresh_token]
        .iter()
        .any(|field| field.is_empty())
    {
        return Err(unavailable());
    }
    if !has_scope(&saved.scope, PLAN_SCOPE) {
        return Err(Error::with(
            "provider_login_unavailable",
            format!("{} was not granted {PLAN_SCOPE}; {HINT}", path.display()),
        ));
    }
    Ok(saved)
}

pub fn has_scope(granted: &str, scope: &str) -> bool {
    granted.split_whitespace().any(|s| s == scope)
}

/// Replace the file whole, readable by its owner only: a reader sees the
/// old token set or the new one, never part of either.
pub fn write(path: &Path, saved: &Saved) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let temporary = dir.join(format!(
        ".{}.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("login"),
        std::process::id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(saved)?)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    Ok(result?)
}

/// The access token a request carries.
#[derive(Clone, PartialEq, Eq)]
pub struct Session {
    pub token: String,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("token", &"[REDACTED]")
            .finish()
    }
}

struct State {
    saved: Saved,
    /// When this process last obtained the held token by refreshing.
    refreshed: Option<Instant>,
}

pub struct Login {
    path: PathBuf,
    token_url: reqwest::Url,
    redaction: Option<Credentials>,
    state: Mutex<State>,
    /// One refresh at a time within the daemon; a file lock covers others.
    refreshing: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for Login {
    /// The path only: a token never enters a debug string.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Login").field("path", &self.path).finish()
    }
}

impl Login {
    /// Read the file once. A missing, malformed or plan-less login fails
    /// `provider_login_unavailable`; an expired access token is refreshed
    /// on first use.
    pub fn open(path: &Path, redaction: Option<Credentials>) -> Result<Self> {
        let saved = read(path)?;
        let login = Self {
            path: path.to_owned(),
            token_url: reqwest::Url::parse(TOKEN_URL).expect("a valid URL"),
            redaction,
            state: Mutex::new(State {
                saved: saved.clone(),
                refreshed: None,
            }),
            refreshing: tokio::sync::Mutex::new(()),
        };
        login.redact(&saved);
        Ok(login)
    }

    /// Send refreshes to another token endpoint; tests use a local one.
    pub fn with_token_url(mut self, url: &str) -> Result<Self> {
        self.token_url =
            reqwest::Url::parse(url).map_err(|_| Error::new("invalid_provider_url"))?;
        Ok(self)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn held(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The token to send now, refreshed first when it is near expiry.
    pub async fn current(&self, client: &reqwest::Client) -> Result<Session> {
        let held = {
            let state = self.held();
            (!state.saved.due()).then(|| state.saved.access_token.clone())
        };
        match held {
            Some(token) => Ok(Session { token }),
            None => {
                let stale = self.held().saved.access_token.clone();
                self.refresh(client, &stale).await
            }
        }
    }

    /// After the provider refused `refused` with a 401: true when another
    /// token is now held and the caller may retry. The refused token is
    /// refreshed once, unless this process obtained it moments ago.
    pub async fn reload(&self, client: &reqwest::Client, refused: &Session) -> Result<bool> {
        {
            let state = self.held();
            if state.saved.access_token != refused.token {
                return Ok(true);
            }
            if state.refreshed.is_some_and(|at| at.elapsed() < FRESH) {
                return Ok(false);
            }
        }
        let session = self.refresh(client, &refused.token).await?;
        Ok(session != *refused)
    }

    /// Replace `stale`, unless another call already has: take the file's
    /// token set when it differs from the held one, else refresh it and
    /// write the replacement before anyone sends it.
    async fn refresh(&self, client: &reqwest::Client, stale: &str) -> Result<Session> {
        let _turn = self.refreshing.lock().await;
        {
            let state = self.held();
            if state.saved.access_token != stale {
                return Ok(Session {
                    token: state.saved.access_token.clone(),
                });
            }
        }
        let _lock = lock(&self.path).await?;
        let on_disk = read(&self.path)?;
        if on_disk.access_token != stale && !on_disk.due() {
            return Ok(self.install(on_disk, false));
        }
        let fresh = tokio::time::timeout(DEADLINE, self.exchange(client, &on_disk))
            .await
            .unwrap_or_else(|_| Err(Error::new("provider_connection_timeout")))?;
        write(&self.path, &fresh)?;
        Ok(self.install(fresh, true))
    }

    async fn exchange(&self, client: &reqwest::Client, saved: &Saved) -> Result<Saved> {
        let body = form(&[
            ("grant_type", "refresh_token"),
            ("client_id", &saved.client_id),
            ("refresh_token", &saved.refresh_token),
            ("resource", RESOURCE),
        ]);
        let response = client
            .post(self.token_url.clone())
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(super::connection_error)?;
        let status = response.status().as_u16();
        let body = bounded(response).await?;
        let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            let code = value["error"].as_str().unwrap_or("refused");
            // A server's trouble keeps the token set for another try, after
            // a backoff: a throttled refresh is not the model's pace, so it
            // is not a 429 the model's pool would retry at once. A refusal
            // of the grant itself needs a new sign-in.
            if status >= 500 || status == 429 {
                return Err(Error {
                    code: match status {
                        429 => "provider_login_throttled".to_owned(),
                        _ => format!("provider_http_{status}"),
                    },
                    detail: Some(format!("refreshing {}: {code}", self.path.display())),
                    facts: None,
                });
            }
            return Err(Error::with(
                "provider_login_expired",
                format!(
                    "{} refused its refresh ({code}); {HINT}",
                    self.path.display()
                ),
            ));
        }
        let field = |name: &str| value[name].as_str().filter(|v| !v.is_empty());
        let (Some(access), Some(refresh)) = (field("access_token"), field("refresh_token")) else {
            return Err(Error::with(
                "invalid_provider_response",
                "a refresh without an access and refresh token",
            ));
        };
        let lifetime = value["expires_in"].as_u64().unwrap_or(3600);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Ok(Saved {
            access_token: access.to_owned(),
            refresh_token: refresh.to_owned(),
            id_token: field("id_token").unwrap_or(&saved.id_token).to_owned(),
            expires_at: now + lifetime,
            scope: field("scope").unwrap_or(&saved.scope).to_owned(),
            ..saved.clone()
        })
    }

    fn install(&self, saved: Saved, refreshed: bool) -> Session {
        self.redact(&saved);
        let token = saved.access_token.clone();
        let mut state = self.held();
        state.saved = saved;
        state.refreshed = refreshed.then(Instant::now);
        Session { token }
    }

    fn redact(&self, saved: &Saved) {
        if let Some(redaction) = &self.redaction {
            redaction.set(TOKEN_NAME, &saved.access_token);
            redaction.set(REFRESH_NAME, &saved.refresh_token);
            redaction.set(ID_NAME, &saved.id_token);
        }
    }

    #[cfg(test)]
    pub(crate) fn expire_now(&self) {
        self.held().saved.expires_at = 0;
    }
}

/// `application/x-www-form-urlencoded`, as the token endpoint takes.
pub fn form(pairs: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("http://form.invalid/").expect("a valid URL");
    url.query_pairs_mut().extend_pairs(pairs);
    url.query().unwrap_or_default().to_owned()
}

async fn bounded(response: reqwest::Response) -> Result<Vec<u8>> {
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(super::connection_error)?;
        if body.len() + chunk.len() > LIMIT {
            return Err(Error::new("provider_response_limit"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// An exclusive lock on `PATH.lock`, held while a refresh reads, exchanges
/// and rewrites the file, so two daemons sharing one login never spend the
/// same refresh token, and while a sign-in replaces it.
pub async fn lock(path: &Path) -> Result<std::fs::File> {
    let path = path.with_extension("json.lock");
    let file = tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: the descriptor is open for the call's duration.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(file)
    })
    .await
    .map_err(|_| Error::new("provider_login_unavailable"))??;
    Ok(file)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A plan login whose access token is `token`, valid for a day.
    pub(crate) fn save(path: &Path, token: &str) {
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 86_400;
        write(
            path,
            &Saved {
                client_id: "oaiapp_synthetic".into(),
                subject: "user-synthetic".into(),
                access_token: token.into(),
                refresh_token: format!("refresh-{token}"),
                id_token: "id-synthetic".into(),
                expires_at: at,
                scope: format!("openid offline_access {PLAN_SCOPE}"),
                email: None,
            },
        )
        .unwrap();
    }

    pub(crate) fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Answer each token request with the next reply, recording its body.
    pub(crate) async fn token_server(
        replies: Vec<(u16, String)>,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        let (seen, bodies) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            for (status, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let n = socket.read(&mut buffer).await.unwrap();
                    request.extend_from_slice(&buffer[..n]);
                    let text = String::from_utf8_lossy(&request).to_string();
                    if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                        let length = head
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(str::to_owned)
                            })
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if rest.len() >= length {
                            let _ = seen.send(rest.to_owned());
                            break;
                        }
                    }
                }
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        (url, bodies)
    }

    #[test]
    fn debug_strings_never_carry_a_token() {
        let session = Session {
            token: "synthetic-secret-bearer".into(),
        };
        assert!(!format!("{session:?}").contains("synthetic-secret-bearer"));
        let dir = temp("login-debug");
        let path = dir.join("chatgpt.json");
        save(&path, "synthetic-secret-bearer");
        let saved = read(&path).unwrap();
        assert!(!format!("{saved:?}").contains("synthetic-secret"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_file_without_the_plan_scope_or_a_token_is_refused() {
        let dir = temp("login-scope");
        let path = dir.join("chatgpt.json");
        assert_eq!(read(&path).unwrap_err().code, "provider_login_unavailable");
        save(&path, "a");
        let mut saved = read(&path).unwrap();
        saved.scope = "openid profile email".into();
        write(&path, &saved).unwrap();
        let error = read(&path).unwrap_err();
        assert!(error.detail.unwrap().contains(PLAN_SCOPE));
        saved.scope = PLAN_SCOPE.into();
        saved.refresh_token.clear();
        write(&path, &saved).unwrap();
        assert_eq!(read(&path).unwrap_err().code, "provider_login_unavailable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(form(&[("a", "b c"), ("d", "e&f=")]), "a=b+c&d=e%26f%3D");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn an_expiring_token_is_refreshed_once_and_written_back() {
        let dir = temp("login-refresh");
        let path = dir.join("chatgpt.json");
        save(&path, "first");
        let credentials = Credentials::default();
        let (url, mut bodies) = token_server(vec![(
            200,
            r#"{"access_token":"second","refresh_token":"refresh-second","expires_in":3600,"token_type":"Bearer"}"#.into(),
        )])
        .await;
        let login = Login::open(&path, Some(credentials.clone()))
            .unwrap()
            .with_token_url(&url)
            .unwrap();
        let client = reqwest::Client::new();
        assert_eq!(login.current(&client).await.unwrap().token, "first");
        login.expire_now();
        // Two calls at once spend the refresh token once.
        let (a, b) = tokio::join!(login.current(&client), login.current(&client));
        assert_eq!(
            (a.unwrap().token, b.unwrap().token),
            ("second".into(), "second".into())
        );
        let body = bodies.recv().await.unwrap();
        assert!(body.contains("grant_type=refresh_token"), "{body}");
        assert!(body.contains("client_id=oaiapp_synthetic"), "{body}");
        assert!(body.contains("refresh_token=refresh-first"), "{body}");
        assert!(
            body.contains("resource=https%3A%2F%2Fapi.openai.com%2Fv1"),
            "{body}"
        );
        assert!(!body.contains("scope="), "{body}");
        let saved = read(&path).unwrap();
        assert_eq!(saved.refresh_token, "refresh-second");
        assert_eq!(saved.client_id, "oaiapp_synthetic");
        assert!(saved.expires() > SystemTime::now() + Duration::from_secs(3000));
        assert_eq!(
            credentials.redact("first second refresh-second".into()),
            "[REDACTED] [REDACTED] [REDACTED]"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_refused_token_is_refreshed_once_then_the_refusal_stands() {
        let dir = temp("login-refused");
        let path = dir.join("chatgpt.json");
        save(&path, "first");
        let (url, _bodies) = token_server(vec![
            (
                200,
                r#"{"access_token":"second","refresh_token":"r2","expires_in":3600}"#.into(),
            ),
            (400, r#"{"error":"refresh_token_reused"}"#.into()),
        ])
        .await;
        let login = Login::open(&path, None)
            .unwrap()
            .with_token_url(&url)
            .unwrap();
        let client = reqwest::Client::new();
        let first = login.current(&client).await.unwrap();
        assert!(login.reload(&client, &first).await.unwrap());
        let second = login.current(&client).await.unwrap();
        assert_eq!(second.token, "second");
        // A stale refusal for the old token: retry with the held one.
        assert!(login.reload(&client, &first).await.unwrap());
        // Refused moments after a refresh: not refreshed again.
        assert!(!login.reload(&client, &second).await.unwrap());
        // Signed in again meanwhile: the file's token set is taken.
        save(&path, "third");
        login.expire_now();
        assert_eq!(login.current(&client).await.unwrap().token, "third");
        // A refused refresh needs a new sign-in, and says so.
        login.expire_now();
        let mut saved = read(&path).unwrap();
        saved.expires_at = 0;
        write(&path, &saved).unwrap();
        let error = login.current(&client).await.unwrap_err();
        assert_eq!(error.code, "provider_login_expired");
        let detail = error.detail.unwrap();
        assert!(
            detail.contains("refresh_token_reused") && detail.contains("agent login chatgpt"),
            "{detail}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn every_token_in_the_file_is_redacted() {
        let dir = temp("login-redact");
        let path = dir.join("chatgpt.json");
        save(&path, "access-synthetic");
        let redaction = Credentials::default();
        Login::open(&path, Some(redaction.clone())).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let shown = redaction.redact(text);
        for token in [
            "access-synthetic",
            "refresh-access-synthetic",
            "id-synthetic",
        ] {
            assert!(!shown.contains(token), "{shown}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_server_error_keeps_the_token_set_for_another_try() {
        let dir = temp("login-5xx");
        let path = dir.join("chatgpt.json");
        save(&path, "first");
        let (url, _bodies) = token_server(vec![
            (503, "{}".into()),
            (429, r#"{"error":"slow_down"}"#.into()),
        ])
        .await;
        let login = Login::open(&path, None)
            .unwrap()
            .with_token_url(&url)
            .unwrap();
        login.expire_now();
        let mut saved = read(&path).unwrap();
        saved.expires_at = 0;
        write(&path, &saved).unwrap();
        let error = login.current(&reqwest::Client::new()).await.unwrap_err();
        assert_eq!(error.code, "provider_http_503");
        // A throttled refresh backs off rather than retrying at the model's pace.
        let error = login.current(&reqwest::Client::new()).await.unwrap_err();
        assert_eq!(error.code, "provider_login_throttled");
        assert_eq!(read(&path).unwrap().refresh_token, "refresh-first");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
