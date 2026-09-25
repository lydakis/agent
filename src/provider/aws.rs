//! Amazon Bedrock: AWS Signature Version 4 and the credentials it signs with.
//! Bedrock speaks the two wire families this runtime already encodes, so the
//! only thing it needs of its own is authentication.
//!
//! Credentials come from the AWS chain in its own order: static keys from the
//! environment first, then everything else the AWS CLI resolves (profiles,
//! SSO, assumed roles, container and instance roles) through its standard
//! `credential_process` output. Temporary keys are re-resolved shortly before
//! they expire and once when the service calls them expired, so a daemon
//! outlives any one session.
//!
//! Requests stream their body from the store, so it is never held to hash:
//! the payload is signed as `UNSIGNED-PAYLOAD` over TLS.
use crate::{Error, Result, tools::Credentials};
use aws_lc_rs::{digest, hmac};
use serde_json::Value;
use std::{
    sync::{Arc, RwLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Re-resolve temporary keys this long before they expire, as AWS SDKs do.
const REFRESH_AHEAD: Duration = Duration::from_secs(300);
/// Bound on one `aws` credential resolution; an SSO login that needs a
/// browser fails here with the CLI's own message instead of hanging a turn.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
/// A refusal re-resolves at most this often, so a fleet refused for another
/// reason (a model not enabled, a missing permission) cannot spawn the CLI
/// once per request.
const RELOAD_INTERVAL: Duration = Duration::from_secs(10);
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// The region and signing name of a Bedrock endpoint, from its host:
/// `bedrock-runtime.{region}.amazonaws.com` signs as `bedrock`,
/// `bedrock-mantle.{region}.api.aws` as `bedrock-mantle`. Anything else is
/// not Bedrock.
pub fn endpoint(url: &reqwest::Url) -> Option<(String, &'static str)> {
    let host = url.host_str()?;
    let (service, region) = match host.strip_prefix("bedrock-runtime.") {
        Some(rest) => ("bedrock", rest.strip_suffix(".amazonaws.com")?),
        None => (
            "bedrock-mantle",
            host.strip_prefix("bedrock-mantle.")?
                .strip_suffix(".api.aws")?,
        ),
    };
    (!region.is_empty()
        && region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
    .then(|| (region.to_owned(), service))
}

/// One set of AWS keys. The secret and session token never enter a debug
/// string.
#[derive(Clone, PartialEq, Eq)]
pub struct Keys {
    pub access: String,
    secret: String,
    pub token: Option<String>,
    /// When temporary keys stop working; `None` for long-term keys.
    pub expires: Option<SystemTime>,
}
impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keys")
            .field("access", &self.access)
            .field("expires", &self.expires)
            .finish_non_exhaustive()
    }
}
impl Keys {
    pub fn new(access: String, secret: String, token: Option<String>) -> Self {
        Self {
            access,
            secret,
            token,
            expires: None,
        }
    }
    fn stale(&self, now: SystemTime) -> bool {
        self.expires.is_some_and(|at| at <= now + REFRESH_AHEAD)
    }
    fn expired(&self, now: SystemTime) -> bool {
        self.expires.is_some_and(|at| at <= now)
    }
}

enum Source {
    /// `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, read once: the
    /// process environment cannot change under a running daemon.
    Environment,
    /// `aws configure export-credentials --format process`, which resolves
    /// the rest of the chain for `AWS_PROFILE` or the default profile.
    Cli,
}

pub struct Aws {
    region: String,
    service: &'static str,
    source: Source,
    keys: RwLock<Arc<Keys>>,
    /// Single flight for re-resolution, and when it last ran.
    refresh: tokio::sync::Mutex<Option<Instant>>,
    redaction: Option<Credentials>,
}
impl std::fmt::Debug for Aws {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Aws")
            .field("region", &self.region)
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

impl Aws {
    /// Resolve credentials once for the Bedrock endpoint at `url`. Fails
    /// `provider_aws_credentials_unavailable` with the reason, so a daemon
    /// never starts with a binding it cannot sign for.
    pub async fn open(url: &reqwest::Url, redaction: Option<Credentials>) -> Result<Self> {
        let (region, service) = endpoint(url).ok_or(Error::new("invalid_provider_url"))?;
        let from_env = std::env::var("AWS_ACCESS_KEY_ID")
            .ok()
            .filter(|v| !v.is_empty())
            .zip(
                std::env::var("AWS_SECRET_ACCESS_KEY")
                    .ok()
                    .filter(|v| !v.is_empty()),
            );
        let (source, keys) = match from_env {
            Some((access, secret)) => {
                let token = std::env::var("AWS_SESSION_TOKEN")
                    .ok()
                    .filter(|v| !v.is_empty());
                (Source::Environment, Keys::new(access, secret, token))
            }
            None => (Source::Cli, resolve().await?),
        };
        if keys.expired(SystemTime::now()) {
            return Err(Error::new("provider_aws_credentials_expired"));
        }
        let aws = Self {
            region,
            service,
            source,
            keys: RwLock::new(Arc::new(Keys::new(String::new(), String::new(), None))),
            refresh: tokio::sync::Mutex::new(None),
            redaction,
        };
        aws.install(keys);
        Ok(aws)
    }

    #[cfg(test)]
    pub(crate) fn fixed(region: &str, service: &'static str, keys: Keys) -> Self {
        Self {
            region: region.to_owned(),
            service,
            source: Source::Environment,
            keys: RwLock::new(Arc::new(keys)),
            refresh: tokio::sync::Mutex::new(None),
            redaction: None,
        }
    }

    pub fn region(&self) -> &str {
        &self.region
    }
    pub fn service(&self) -> &'static str {
        self.service
    }
    /// Where the keys come from, as `ready.providers` reports it.
    pub fn source(&self) -> &'static str {
        match self.source {
            Source::Environment => "environment",
            Source::Cli => "aws-cli",
        }
    }

    fn held(&self) -> Arc<Keys> {
        self.keys.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn install(&self, keys: Keys) {
        if let Some(redaction) = &self.redaction {
            let names = match self.source {
                Source::Environment => ["AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN"],
                Source::Cli => ["AGENT_AWS_SECRET_ACCESS_KEY", "AGENT_AWS_SESSION_TOKEN"],
            };
            redaction.set(names[0], &keys.secret);
            if let Some(token) = &keys.token {
                redaction.set(names[1], token);
            }
        }
        *self.keys.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(keys);
    }

    /// The keys to sign with now. Keys inside the refresh window are
    /// re-resolved first, by one caller at a time; while they still work, a
    /// failed re-resolution keeps them, and once they have expired it fails
    /// the request.
    pub async fn current(&self) -> Result<Arc<Keys>> {
        let held = self.held();
        let now = SystemTime::now();
        if !held.stale(now) || matches!(self.source, Source::Environment) {
            return match held.expired(now) {
                true => Err(Error::new("provider_aws_credentials_expired")),
                false => Ok(held),
            };
        }
        let mut last = self.refresh.lock().await;
        let held = self.held();
        if !held.stale(SystemTime::now()) {
            return Ok(held);
        }
        *last = Some(Instant::now());
        match resolve().await {
            Ok(fresh) => {
                self.install(fresh);
                Ok(self.held())
            }
            Err(_) if !held.expired(SystemTime::now()) => Ok(held),
            Err(error) => Err(error),
        }
    }

    /// After the service refused `refused` as expired or unrecognized: use
    /// keys another call has already installed, or re-resolve. True when
    /// different keys are now held, so the caller may retry.
    pub async fn reload(&self, refused: &Keys) -> Result<bool> {
        if matches!(self.source, Source::Environment) {
            return Ok(false);
        }
        let mut last = self.refresh.lock().await;
        if *self.held() != *refused {
            return Ok(true);
        }
        if last.is_some_and(|at| at.elapsed() < RELOAD_INTERVAL) {
            return Ok(false);
        }
        *last = Some(Instant::now());
        let fresh = resolve().await?;
        let changed = fresh != *refused;
        if changed {
            self.install(fresh);
        }
        Ok(changed)
    }

    /// The headers that sign one request to `url` at `now`.
    pub fn sign(
        &self,
        keys: &Keys,
        method: &str,
        url: &reqwest::Url,
        now: SystemTime,
    ) -> Vec<(&'static str, String)> {
        sign(
            keys,
            &self.region,
            self.service,
            method,
            url,
            now,
            UNSIGNED_PAYLOAD,
        )
    }
}

/// Run the AWS CLI's credential export. Its stdout holds the secret, so it
/// never becomes a diagnostic; its stderr is the CLI's own explanation.
async fn resolve() -> Result<Keys> {
    let unavailable = |why: String| Error::with("provider_aws_credentials_unavailable", why);
    let mut command = tokio::process::Command::new("aws");
    command
        .args(["configure", "export-credentials", "--format", "process"])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(RESOLVE_TIMEOUT, command.output())
        .await
        .map_err(|_| unavailable("aws configure export-credentials timed out".into()))?
        .map_err(|error| {
            unavailable(match error.kind() {
                std::io::ErrorKind::NotFound => {
                    "no AWS keys in the environment and no aws CLI on PATH".into()
                }
                _ => format!("could not run aws: {error}"),
            })
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr.trim();
        return Err(unavailable(if reason.is_empty() {
            format!("aws configure export-credentials exited {}", output.status)
        } else {
            reason.chars().take(512).collect()
        }));
    }
    parse_process(&output.stdout)
        .ok_or_else(|| unavailable("aws configure export-credentials printed no keys".into()))
}

/// The `credential_process` JSON: version 1, the key pair, an optional
/// session token and an optional RFC 3339 expiration.
fn parse_process(stdout: &[u8]) -> Option<Keys> {
    let value: Value = serde_json::from_slice(stdout).ok()?;
    if value["Version"].as_u64() != Some(1) {
        return None;
    }
    let text = |key: &str| {
        value[key]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let mut keys = Keys::new(
        text("AccessKeyId")?,
        text("SecretAccessKey")?,
        text("SessionToken"),
    );
    keys.expires = match value.get("Expiration") {
        None | Some(Value::Null) => None,
        Some(at) => Some(rfc3339(at.as_str()?)?),
    };
    Some(keys)
}

/// `2026-09-25T02:00:00Z`, `…+00:00`, or with fractional seconds.
fn rfc3339(text: &str) -> Option<SystemTime> {
    let (date, time) = text.trim().split_once(['T', 't', ' '])?;
    let mut parts = date.splitn(3, '-');
    let (year, month, day): (i64, i64, i64) = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    let (clock, offset) = time.split_at(time.find(['Z', 'z', '+', '-'])?);
    let offset = match offset {
        "Z" | "z" => 0,
        _ => {
            let sign = if offset.starts_with('-') { -1 } else { 1 };
            let (hours, minutes) = offset[1..].split_once(':')?;
            sign * (hours.parse::<i64>().ok()? * 3600 + minutes.parse::<i64>().ok()? * 60)
        }
    };
    let mut clock = clock.splitn(3, ':');
    let (hour, minute): (i64, i64) = (clock.next()?.parse().ok()?, clock.next()?.parse().ok()?);
    let seconds = clock.next()?;
    let whole: i64 = seconds.split('.').next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    let secs =
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + whole - offset;
    Some(UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).ok()?))
}

/// Days since 1970-01-01, Howard Hinnant's algorithm.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYYMMDDTHHMMSSZ` for `now` in UTC.
fn amz_date(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil from days, Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 15)] as char);
    }
    out
}

fn mac(key: &[u8], data: &[u8]) -> hmac::Tag {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data)
}

/// Percent-encode a request path as SigV4 canonicalizes it for services
/// other than S3: the path as sent, with every byte outside the unreserved
/// set and `/` encoded again.
fn canonical_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    if out.is_empty() { "/".into() } else { out }
}

/// The request signature: the canonical request's digest in the string to
/// sign, under the key derived from the secret for this day, region and
/// service.
fn signature(secret: &str, stamp: &str, region: &str, service: &str, canonical: &str) -> String {
    let date = &stamp[..8];
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{stamp}\n{date}/{region}/{service}/aws4_request\n{}",
        hex(digest::digest(&digest::SHA256, canonical.as_bytes()).as_ref())
    );
    let key = [region, service, "aws4_request"].iter().fold(
        mac(format!("AWS4{secret}").as_bytes(), date.as_bytes()),
        |key, part| mac(key.as_ref(), part.as_bytes()),
    );
    hex(mac(key.as_ref(), to_sign.as_bytes()).as_ref())
}

/// SigV4 header signing for a request with no query string: `host`,
/// `x-amz-content-sha256`, `x-amz-date` and, with temporary keys,
/// `x-amz-security-token` are signed.
fn sign(
    keys: &Keys,
    region: &str,
    service: &str,
    method: &str,
    url: &reqwest::Url,
    now: SystemTime,
    payload: &str,
) -> Vec<(&'static str, String)> {
    let stamp = amz_date(now);
    let date = &stamp[..8];
    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_owned(),
    };
    let mut headers: Vec<(&'static str, String)> = vec![
        ("host", host),
        ("x-amz-content-sha256", payload.to_owned()),
        ("x-amz-date", stamp.clone()),
    ];
    if let Some(token) = &keys.token {
        headers.push(("x-amz-security-token", token.clone()));
    }
    let signed = headers
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(";");
    let mut canonical = format!(
        "{method}\n{}\n{}\n",
        canonical_path(url.path()),
        url.query().unwrap_or_default()
    );
    for (name, value) in &headers {
        canonical.push_str(name);
        canonical.push(':');
        canonical.push_str(value.trim());
        canonical.push('\n');
    }
    canonical.push('\n');
    canonical.push_str(&signed);
    canonical.push('\n');
    canonical.push_str(payload);
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let signature = signature(&keys.secret, &stamp, region, service, &canonical);
    headers.remove(0); // The client sends `host` itself.
    headers.push((
        "authorization",
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
            keys.access
        ),
    ));
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn bedrock_hosts_name_their_region_and_signing_name() {
        let url = |u: &str| reqwest::Url::parse(u).unwrap();
        assert_eq!(
            endpoint(&url(
                "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1"
            )),
            Some(("us-east-1".into(), "bedrock-mantle"))
        );
        assert_eq!(
            endpoint(&url(
                "https://bedrock-runtime.eu-west-1.amazonaws.com/openai/v1"
            )),
            Some(("eu-west-1".into(), "bedrock"))
        );
        for other in [
            "https://api.anthropic.com/v1",
            "https://bedrock-runtime.amazonaws.com/x",
            "https://bedrock-mantle.us-east-1.example.com/x",
            "https://bedrock-runtime.US-EAST-1.evil.test.amazonaws.com.attacker/x",
        ] {
            assert_eq!(endpoint(&url(other)), None, "{other}");
        }
    }

    #[test]
    fn dates_format_in_utc_and_parse_with_offsets() {
        assert_eq!(amz_date(at(1_440_938_160)), "20150830T123600Z");
        assert_eq!(amz_date(at(951_782_400)), "20000229T000000Z");
        assert_eq!(rfc3339("2015-08-30T12:36:00Z"), Some(at(1_440_938_160)));
        assert_eq!(
            rfc3339("2015-08-30T12:36:00.123+00:00"),
            Some(at(1_440_938_160))
        );
        assert_eq!(
            rfc3339("2015-08-30T14:36:00+02:00"),
            Some(at(1_440_938_160))
        );
        assert_eq!(rfc3339("2015-08-30T12:36:00"), None);
        assert_eq!(rfc3339("not a date"), None);
    }

    /// The AWS SigV4 test suite's `get-vanilla` and `post-vanilla` cases,
    /// whose published signatures fix the string to sign and key chain.
    #[test]
    fn signatures_match_the_aws_test_suite() {
        let empty = hex(digest::digest(&digest::SHA256, b"").as_ref());
        for (method, expected) in [
            (
                "GET",
                "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31",
            ),
            (
                "POST",
                "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b",
            ),
        ] {
            let canonical = format!(
                "{method}\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\n{empty}"
            );
            assert_eq!(
                signature(
                    "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
                    "20150830T123600Z",
                    "us-east-1",
                    "service",
                    &canonical
                ),
                expected
            );
        }
    }

    /// Whole requests as botocore 1.43 signs them with payload signing off,
    /// with and without a session token.
    #[test]
    fn requests_sign_as_botocore_signs_them() {
        let now = at(1_790_296_697);
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let cases = [
            (
                "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1/messages",
                None,
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260925/us-east-1/bedrock-mantle/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=0e2767eece6a1fbd3aab62fc4f351e7c13b3ecb410f51377592fd988f9322d81",
            ),
            (
                "https://bedrock-runtime.eu-west-1.amazonaws.com/openai/v1/responses",
                Some("FQoGZXIvYXdzEXAMPLE+/token="),
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260925/eu-west-1/bedrock/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token, Signature=af399be6054a5e9a0972d447fa9b21046857282c7ec80616c6c54fadb448e39f",
            ),
        ];
        for (url, token, expected) in cases {
            let url = reqwest::Url::parse(url).unwrap();
            let (region, service) = endpoint(&url).unwrap();
            let aws = Aws::fixed(
                &region,
                service,
                Keys::new(
                    "AKIDEXAMPLE".into(),
                    secret.into(),
                    token.map(str::to_owned),
                ),
            );
            let headers = aws.sign(&aws.held(), "POST", &url, now);
            let get = |name: &str| {
                headers
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| v.as_str())
            };
            assert_eq!(get("authorization"), Some(expected));
            assert_eq!(get("x-amz-date"), Some("20260925T003817Z"));
            assert_eq!(get("x-amz-content-sha256"), Some(UNSIGNED_PAYLOAD));
            assert_eq!(get("x-amz-security-token"), token);
            assert_eq!(get("host"), None);
        }
    }

    #[test]
    fn credential_process_output_is_parsed_and_secrets_stay_out_of_debug() {
        let keys = parse_process(
            br#"{"Version":1,"AccessKeyId":"AKIDEXAMPLE","SecretAccessKey":"s3cr3t","SessionToken":"t0k3n","Expiration":"2015-08-30T12:36:00Z"}"#,
        )
        .unwrap();
        assert_eq!(keys.access, "AKIDEXAMPLE");
        assert_eq!(keys.token.as_deref(), Some("t0k3n"));
        assert_eq!(keys.expires, Some(at(1_440_938_160)));
        let debug = format!("{keys:?}");
        assert!(
            !debug.contains("s3cr3t") && !debug.contains("t0k3n"),
            "{debug}"
        );
        let long =
            parse_process(br#"{"Version":1,"AccessKeyId":"A","SecretAccessKey":"S"}"#).unwrap();
        assert_eq!((long.token, long.expires), (None, None));
        for bad in [
            &br#"{"Version":2,"AccessKeyId":"A","SecretAccessKey":"S"}"#[..],
            br#"{"Version":1,"AccessKeyId":"A"}"#,
            br#"{"Version":1,"AccessKeyId":"A","SecretAccessKey":"S","Expiration":"soon"}"#,
            b"not json",
        ] {
            assert!(parse_process(bad).is_none());
        }
        assert!(keys.stale(at(1_440_938_160 - 299)));
        assert!(!keys.stale(at(1_440_938_160 - 301)));
        assert!(keys.expired(at(1_440_938_160)));
    }

    #[test]
    fn paths_are_encoded_again_as_sigv4_canonicalizes_them() {
        assert_eq!(
            canonical_path("/anthropic/v1/messages"),
            "/anthropic/v1/messages"
        );
        assert_eq!(canonical_path("/a%20b/c:d"), "/a%2520b/c%3Ad");
        assert_eq!(canonical_path(""), "/");
    }
}
