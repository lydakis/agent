//! A ChatGPT login read from Codex's `auth.json`: the access token and
//! workspace a request carries. Codex refreshes the file when it runs; this
//! never refreshes anything itself. It re-reads the file when the token it
//! holds has expired or a request was refused, so a daemon outlives one
//! token as long as Codex keeps signing in, and it says when it cannot.
use crate::{Error, Result, tools::Credentials};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// The environment name the token is redacted under; no variable carries it.
pub const TOKEN_NAME: &str = "AGENT_CHATGPT_TOKEN";

#[derive(Clone, PartialEq, Eq)]
pub struct Session {
    pub token: String,
    pub account: String,
    /// From the token's `exp` claim when it is a JWT; unknown otherwise.
    pub expires: Option<SystemTime>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("token", &"[REDACTED]")
            .field("account", &self.account)
            .field("expires", &self.expires)
            .finish()
    }
}

pub struct Login {
    path: PathBuf,
    redaction: Option<Credentials>,
    state: Mutex<Session>,
}

impl std::fmt::Debug for Login {
    /// The path only: a token never enters a debug string.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Login").field("path", &self.path).finish()
    }
}

impl Login {
    /// Read the file once. A missing or malformed login fails
    /// `provider_login_unavailable`; an already expired token fails
    /// `provider_login_expired` and names the time.
    pub fn open(path: &Path, redaction: Option<Credentials>) -> Result<Self> {
        let session = read(path)?;
        if let Some(at) = session.expires.filter(|at| *at <= SystemTime::now()) {
            return Err(expired(path, at));
        }
        if let Some(redaction) = &redaction {
            redaction.set(TOKEN_NAME, &session.token);
        }
        Ok(Self {
            path: path.to_owned(),
            redaction,
            state: Mutex::new(session),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The session to send now. An expired one is re-read first, and the
    /// file's token has to be current for the request to go out at all.
    pub fn current(&self) -> Result<Session> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.expires.is_some_and(|at| at <= SystemTime::now()) {
            let fresh = read(&self.path)?;
            if let Some(at) = fresh.expires.filter(|at| *at <= SystemTime::now()) {
                return Err(expired(&self.path, at));
            }
            self.install(&mut state, fresh);
        }
        Ok(state.clone())
    }

    /// After the provider refused `refused`: use a session another call has
    /// already installed, or re-read the file. True when a different token or
    /// account is now held, so the caller may retry.
    pub fn reload(&self, refused: &Session) -> Result<bool> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.token != refused.token || state.account != refused.account {
            return Ok(true);
        }
        let fresh = read(&self.path)?;
        if fresh.token != state.token || fresh.account != state.account {
            self.install(&mut state, fresh);
            return Ok(true);
        }
        Ok(false)
    }

    fn install(&self, state: &mut Session, fresh: Session) {
        if let Some(redaction) = &self.redaction
            && fresh.token != state.token
        {
            redaction.set(TOKEN_NAME, &fresh.token);
        }
        *state = fresh;
    }

    #[cfg(test)]
    pub(crate) fn expire_now(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).expires = Some(UNIX_EPOCH);
    }
}

/// The access token and workspace from Codex's `auth.json`.
pub fn read(path: &Path) -> Result<Session> {
    let unavailable = || Error::with("provider_login_unavailable", path.display().to_string());
    let auth: Value = serde_json::from_slice(&std::fs::read(path).map_err(|_| unavailable())?)
        .map_err(|_| unavailable())?;
    let tokens = &auth["tokens"];
    match (
        tokens["access_token"].as_str(),
        tokens["account_id"].as_str(),
    ) {
        (Some(token), Some(account)) if !token.is_empty() && !account.is_empty() => Ok(Session {
            token: token.to_owned(),
            account: account.to_owned(),
            expires: jwt_expiry(token),
        }),
        _ => Err(unavailable()),
    }
}

fn expired(path: &Path, at: SystemTime) -> Error {
    Error::with(
        "provider_login_expired",
        format!(
            "{} expired at {}; run any codex command to refresh it",
            path.display(),
            iso8601(at)
        ),
    )
}

/// The `exp` claim of a JWT, without verifying it: this schedules a re-read,
/// it does not decide whether the provider will accept the token.
fn jwt_expiry(token: &str) -> Option<SystemTime> {
    let payload = token.split('.').nth(1)?;
    let claims: Value = serde_json::from_slice(&base64url(payload)?).ok()?;
    let exp = claims["exp"].as_u64()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(exp))
}

fn base64url(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut bits, mut have) = (0u32, 0u32);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        have += 6;
        if have >= 8 {
            have -= 8;
            out.push((bits >> have) as u8);
            bits &= (1 << have) - 1;
        }
    }
    Some(out)
}

/// `YYYY-MM-DDTHH:MM:SSZ` from a system time, for a human reading an error.
fn iso8601(at: SystemTime) -> String {
    let secs = at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant), proleptic Gregorian.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_debug_never_exposes_the_access_token() {
        let session = Session {
            token: "synthetic-secret-bearer".into(),
            account: "workspace".into(),
            expires: None,
        };
        let debug = format!("{session:?}");
        assert!(debug.contains("Session"));
        assert!(!debug.contains("synthetic-secret-bearer"));
    }

    fn jwt(claims: &str) -> String {
        let encode = |bytes: &[u8]| {
            const TABLE: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let n = chunk
                    .iter()
                    .enumerate()
                    .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
                for i in 0..=chunk.len() {
                    out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
                }
            }
            out
        };
        format!(
            "{}.{}.sig",
            encode(br#"{"alg":"RS256"}"#),
            encode(claims.as_bytes())
        )
    }

    fn write(path: &Path, token: &str) {
        write_account(path, token, "w");
    }

    fn write_account(path: &Path, token: &str, account: &str) {
        std::fs::write(
            path,
            format!(
                r#"{{"tokens":{{"access_token":"{token}","account_id":"{account}","refresh_token":"r"}}}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn expiry_comes_from_the_token_and_is_named_in_iso8601() {
        assert_eq!(iso8601(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            iso8601(UNIX_EPOCH + Duration::from_secs(1_790_000_000)),
            "2026-09-21T14:13:20Z"
        );
        let at = jwt_expiry(&jwt(r#"{"exp":1790000000,"sub":"x"}"#)).unwrap();
        assert_eq!(at, UNIX_EPOCH + Duration::from_secs(1_790_000_000));
        assert_eq!(jwt_expiry("opaque-token"), None);
        assert_eq!(jwt_expiry(&jwt(r#"{"sub":"x"}"#)), None);
        assert_eq!(base64url("aGk=").unwrap(), b"hi");
        assert_eq!(base64url("a?"), None);
    }

    #[test]
    fn a_login_is_reread_when_expired_or_refused_and_redaction_follows() {
        let dir = std::env::temp_dir().join(format!("agent-login-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        let credentials = Credentials::default();
        write(&path, &jwt(r#"{"exp":1}"#));
        let error = Login::open(&path, None).unwrap_err();
        assert_eq!(error.code, "provider_login_expired");
        assert!(
            error
                .detail
                .as_deref()
                .unwrap()
                .contains("expired at 1970-01-01T00:00:01Z; run any codex command"),
            "{error:?}"
        );
        write(&path, "first");
        let login = Login::open(&path, Some(credentials.clone())).unwrap();
        assert_eq!(login.current().unwrap().token, "first");
        assert_eq!(
            credentials.redact("token first here".into()),
            "token [REDACTED] here"
        );
        // Refused and unchanged on disk: nothing to retry with.
        let first = login.current().unwrap();
        assert!(!login.reload(&first).unwrap());
        // Refused, and Codex has since written a new token: retry with it.
        write(&path, "second");
        assert!(login.reload(&first).unwrap());
        assert_eq!(login.current().unwrap().token, "second");
        assert_eq!(
            credentials.redact("first second".into()),
            "[REDACTED] [REDACTED]"
        );
        // Another turn already reloaded: a stale refusal still finds a new token.
        assert!(login.reload(&first).unwrap());
        // Expired in memory: re-read before sending, and refuse a stale file.
        write(&path, &jwt(r#"{"exp":4102444800}"#));
        login.expire_now();
        let refreshed = login.current().unwrap();
        assert!(refreshed.token.starts_with("eyJ"));
        write(&path, &jwt(r#"{"exp":1}"#));
        login.expire_now();
        assert_eq!(login.current().unwrap_err().code, "provider_login_expired");
        std::fs::remove_dir_all(&dir).unwrap();
        // A late 401 for an older token should reuse the installed login,
        // without re-reading a file that may since have moved or disappeared.
        assert!(login.reload(&first).unwrap());
        assert_eq!(
            login.reload(&refreshed).unwrap_err().code,
            "provider_login_unavailable"
        );
    }

    #[test]
    fn a_refused_login_picks_up_an_account_change_with_the_same_token() {
        let dir = std::env::temp_dir().join(format!("agent-login-account-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        write_account(&path, "same-token", "old-account");
        let login = Login::open(&path, None).unwrap();
        let refused = login.current().unwrap();
        write_account(&path, "same-token", "new-account");
        assert!(login.reload(&refused).unwrap());
        let current = login.current().unwrap();
        assert_eq!(current.token, "same-token");
        assert_eq!(current.account, "new-account");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
