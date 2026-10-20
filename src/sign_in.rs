//! `agent login chatgpt`: Sign in with ChatGPT for the `chatgpt` provider
//! (developers.openai.com/siwc, read 2026-10-20). It runs in this process,
//! never the daemon's: the browser's callback reaches 127.0.0.1 on the
//! machine with the browser. It writes `~/.agent/chatgpt.json`, which the
//! daemon reads and refreshes from then on.
use agent_runtime::provider::login::{self, PLAN_SCOPE, Saved};
use agent_runtime::{Error, Result, fail_with};
use aws_lc_rs::{digest, rand::SecureRandom, signature};
use base64::Engine;
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
const DISCOVERY: &str = "https://auth.openai.com/.well-known/openid-configuration";
/// The first sign-in registers a client for this user and workspace; later
/// sign-ins and every refresh use the client it issued.
const REGISTER: &str = "dynamic_agent_client";
/// The name OpenAI shows for the registered client, the same on every install.
const NAME: &str = "Agent";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const PORT: u16 = 1455;
const WAIT: Duration = Duration::from_secs(600);
const DEADLINE: Duration = Duration::from_secs(30);
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// `login chatgpt [--port N] [--pretty]`, as `cli::prepare` checked it.
pub fn main(args: &[String]) -> Result<i32> {
    let mut port = PORT;
    let mut pretty = false;
    let mut providers = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--pretty" => pretty = true,
            "--port" => {
                let value = iter.next().map(String::as_str).unwrap_or_default();
                port = value
                    .parse::<u16>()
                    .ok()
                    .filter(|port| *port > 0)
                    .ok_or(Error::with("usage", "--port needs a port from 1 to 65535"))?;
            }
            // Connection flags name a daemon, which signing in never reaches.
            "--store" | "--socket" => {
                iter.next();
            }
            provider => providers.push(provider),
        }
    }
    if providers != ["chatgpt"] {
        return fail_with("usage", "login needs a provider: chatgpt");
    }
    let path = login::default_path()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let signed = runtime.block_on(sign_in(&path, port))?;
    if pretty {
        let who = signed.email.as_deref().unwrap_or("your ChatGPT account");
        println!(
            "Signed in as {who}; the chatgpt provider uses {}",
            path.display()
        );
    } else {
        println!(
            "{}",
            json!({"provider":"chatgpt","file":path,"email":signed.email,
                "expires_at":signed.expires_at})
        );
    }
    Ok(0)
}

async fn sign_in(path: &Path, port: u16) -> Result<Saved> {
    let host = host_id(&path.with_file_name("host-id"))?;
    // Signed in before: sign in again as that client, which keeps its
    // workspace; with no file, register a new one.
    // A file that cannot be read as a login is not taken for none: that
    // would register anew over its client and account.
    let previous: Option<Saved> = match std::fs::read(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return fail_with("login_unreadable", format!("{}: {error}", path.display())),
        Ok(bytes) => Some(serde_json::from_slice(&bytes).map_err(|_| {
            Error::with(
                "login_unreadable",
                format!(
                    "{} is not a login; delete it to sign in anew",
                    path.display()
                ),
            )
        })?),
    };
    let verifier = random(32)?;
    let challenge = B64.encode(digest::digest(&digest::SHA256, verifier.as_bytes()));
    let (state, nonce) = (random(16)?, random(16)?);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|error| {
            Error::with(
                "login_port_unavailable",
                format!("127.0.0.1:{port}: {error}; pass --port with a free one"),
            )
        })?;
    let redirect = format!("http://127.0.0.1:{port}/auth/callback");
    let authorize = Authorize {
        client_id: previous.as_ref().map(|p| p.client_id.as_str()),
        id_token_hint: previous.as_ref().map(|p| p.id_token.as_str()),
        host: &host,
        redirect: &redirect,
        state: &state,
        nonce: &nonce,
        challenge: &challenge,
    };
    let url = authorize_url(&authorize);
    // The printed address leaves out the ID token hint: terminals and their
    // logs keep what is printed, and the hint is optional.
    let shown = authorize_url(&Authorize {
        id_token_hint: None,
        ..authorize
    });
    eprintln!("Open this address to sign in with ChatGPT:\n{shown}");
    if std::env::var_os("SSH_CONNECTION").is_some() {
        eprintln!(
            "Over SSH the browser's callback needs this port forwarded \
             (ssh -L {port}:127.0.0.1:{port} HOST), or sign in on your own \
             machine and copy {} here.",
            path.display()
        );
    }
    open_browser(url.as_str());
    let callback = tokio::time::timeout(WAIT, receive(&listener, &state))
        .await
        .map_err(|_| Error::with("login_timeout", "no sign-in arrived within 10 minutes"))??;
    drop(listener);
    let client_id = match (&previous, callback.client_id) {
        (Some(previous), Some(issued)) if issued != previous.client_id => {
            return fail_with(
                "login_client_mismatch",
                format!(
                    "the sign-in returned another client than {} holds; delete it to register anew",
                    path.display()
                ),
            );
        }
        (Some(previous), _) => previous.client_id.clone(),
        (None, Some(issued)) => issued,
        (None, None) => {
            return fail_with("login_failed", "the sign-in returned no client_id");
        }
    };
    let client = reqwest::Client::builder()
        .user_agent(concat!("agent-runtime/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .timeout(DEADLINE)
        .build()
        .map_err(|_| Error::new("http_client_init"))?;
    let tokens = post_form(
        &client,
        login::TOKEN_URL,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", &client_id),
            ("code", &callback.code),
            ("code_verifier", &verifier),
            ("redirect_uri", &redirect),
            ("resource", login::RESOURCE),
        ],
    )
    .await?;
    let text = |name: &str| tokens[name].as_str().filter(|v| !v.is_empty());
    let (Some(access), Some(refresh), Some(id_token)) = (
        text("access_token"),
        text("refresh_token"),
        text("id_token"),
    ) else {
        return fail_with(
            "login_failed",
            "the token endpoint returned no access, refresh and ID token",
        );
    };
    let discovery = get_json(&client, DISCOVERY).await?;
    let issuer = discovery["issuer"].as_str().unwrap_or(login::ISSUER);
    let jwks_uri = discovery["jwks_uri"]
        .as_str()
        .ok_or(Error::with("login_failed", "the issuer names no jwks_uri"))?;
    let keys = get_json(&client, jwks_uri).await?;
    let now = now();
    let claims = validate_id_token(id_token, &keys, issuer, &client_id, &nonce, now)?;
    let subject = claims["sub"]
        .as_str()
        .filter(|sub| !sub.is_empty())
        .ok_or(Error::with("login_id_token_invalid", "no subject"))?;
    // A sign-in as the saved client replaces only that account's login.
    if let Some(previous) = &previous
        && previous.subject != subject
    {
        return fail_with(
            "login_account_changed",
            format!(
                "the sign-in chose another account than {} holds; delete it to sign in as another",
                path.display()
            ),
        );
    }
    let scope = text("scope")
        .map(str::to_owned)
        .or(callback.scope)
        .unwrap_or_default();
    let saved = Saved {
        client_id,
        subject: subject.to_owned(),
        access_token: access.to_owned(),
        refresh_token: refresh.to_owned(),
        id_token: id_token.to_owned(),
        expires_at: now + tokens["expires_in"].as_u64().unwrap_or(3600),
        scope,
        email: claims["email"].as_str().map(str::to_owned),
    };
    // A valid ID token alone does not let the token spend the plan. A first
    // sign-in is kept anyway, so the next one reuses the client it registered;
    // the daemon refuses a login without the scope.
    let granted = login::has_scope(&saved.scope, PLAN_SCOPE);
    if granted || previous.is_none() {
        // Not while a daemon is refreshing, which would write over this.
        let _lock = login::lock(path).await?;
        login::write(path, &saved)?;
    }
    if !granted {
        return fail_with(
            "login_plan_not_granted",
            format!(
                "the sign-in did not grant {PLAN_SCOPE}; plan use needs ChatGPT Go, Plus or Pro"
            ),
        );
    }
    Ok(saved)
}

struct Authorize<'a> {
    /// The issued client when signing in again; registration otherwise.
    client_id: Option<&'a str>,
    id_token_hint: Option<&'a str>,
    host: &'a str,
    redirect: &'a str,
    state: &'a str,
    nonce: &'a str,
    challenge: &'a str,
}

fn authorize_url(a: &Authorize<'_>) -> reqwest::Url {
    let mut url = reqwest::Url::parse(AUTHORIZE).expect("a valid URL");
    {
        let mut query = url.query_pairs_mut();
        match a.client_id {
            Some(client) => query.append_pair("client_id", client),
            None => query
                .append_pair("client_id", REGISTER)
                .append_pair("agent_name_hint", NAME),
        };
        if let Some(hint) = a.id_token_hint.filter(|hint| !hint.is_empty()) {
            query.append_pair("id_token_hint", hint);
        }
        query
            .append_pair("ext_agent_host_id", a.host)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", a.redirect)
            .append_pair("scope", SCOPE)
            .append_pair("resource", login::RESOURCE)
            .append_pair("state", a.state)
            .append_pair("nonce", a.nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", a.challenge);
    }
    url
}

#[derive(Debug, PartialEq)]
struct Callback {
    code: String,
    scope: Option<String>,
    client_id: Option<String>,
}

/// The browser's request to `/auth/callback` carrying this sign-in's
/// state. Other paths get a 404 and the wait goes on; a wrong state or a
/// refusal ends the sign-in.
async fn receive(listener: &tokio::net::TcpListener, state: &str) -> Result<Callback> {
    loop {
        let (mut socket, _) = listener.accept().await?;
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        // A browser's speculative connection may send nothing; it must not
        // hold up the one that carries the callback.
        let head = async {
            while !request.windows(4).any(|w| w == b"\r\n\r\n") && request.len() < 16 * 1024 {
                match socket.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buffer[..n]),
                }
            }
        };
        if tokio::time::timeout(Duration::from_secs(5), head)
            .await
            .is_err()
        {
            continue;
        }
        let line = String::from_utf8_lossy(&request);
        let target = line
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("GET "))
            .and_then(|rest| rest.split(' ').next())
            .unwrap_or_default();
        let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}")).ok();
        let Some(url) = url.filter(|url| url.path() == "/auth/callback") else {
            reply(&mut socket, "404 Not Found", "Not found.").await;
            continue;
        };
        let param = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
        };
        let outcome = if param("state").as_deref() != Some(state) {
            Err(Error::with(
                "login_failed",
                "the callback's state did not match",
            ))
        } else if let Some(error) = param("error") {
            let why = param("error_description").unwrap_or_default();
            Err(Error::with(
                "login_denied",
                format!("{error} {why}").trim_end(),
            ))
        } else {
            match param("code") {
                Some(code) if !code.is_empty() => Ok(Callback {
                    code,
                    scope: param("scope"),
                    client_id: param("client_id"),
                }),
                _ => Err(Error::with("login_failed", "the callback carried no code")),
            }
        };
        let page = match &outcome {
            Ok(_) => "Signed in. You can close this tab.",
            Err(_) => "The sign-in did not finish; see the terminal.",
        };
        reply(&mut socket, "200 OK", page).await;
        return outcome;
    }
}

async fn reply(socket: &mut tokio::net::TcpStream, status: &str, text: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
        text.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
}

/// An RS256 ID token from `issuer` for `client_id` with this sign-in's
/// nonce, signed by one of `keys`; its claims.
fn validate_id_token(
    token: &str,
    keys: &Value,
    issuer: &str,
    client_id: &str,
    nonce: &str,
    now: u64,
) -> Result<Value> {
    let invalid = |why: &str| Error::with("login_id_token_invalid", why);
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid("not a signed JWT"));
    };
    let decode = |part: &str| B64.decode(part).map_err(|_| invalid("not base64url"));
    let header: Value = serde_json::from_slice(&decode(header)?)?;
    if header["alg"] != "RS256" {
        return Err(invalid("not signed with RS256"));
    }
    let key = keys["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|key| key["kty"] == "RSA" && key["kid"] == header["kid"])
        .ok_or_else(|| invalid("signed by a key the issuer does not list"))?;
    let component = |name: &str| {
        key[name]
            .as_str()
            .ok_or_else(|| invalid("a key without n and e"))
            .and_then(decode)
    };
    let public = signature::RsaPublicKeyComponents {
        n: component("n")?,
        e: component("e")?,
    };
    let signed = &token[..header_len(token)];
    public
        .verify(
            &signature::RSA_PKCS1_2048_8192_SHA256,
            signed.as_bytes(),
            &decode(signature)?,
        )
        .map_err(|_| invalid("the signature does not verify"))?;
    let claims: Value = serde_json::from_slice(&decode(payload)?)?;
    if claims["iss"] != issuer {
        return Err(invalid("issued by another issuer"));
    }
    let audience = match &claims["aud"] {
        Value::String(aud) => aud == client_id,
        Value::Array(auds) => auds.iter().any(|aud| aud == client_id),
        _ => false,
    };
    if !audience {
        return Err(invalid("issued for another client"));
    }
    if claims["exp"].as_u64().is_none_or(|exp| exp <= now) {
        return Err(invalid("expired"));
    }
    if claims["nonce"] != nonce {
        return Err(invalid("the nonce does not match this sign-in"));
    }
    Ok(claims)
}

/// The length of `header.payload`, the part a JWT signature covers.
fn header_len(token: &str) -> usize {
    token.rfind('.').unwrap_or(token.len())
}

/// This machine's opaque host id, made once and kept beside the login, so
/// a login copied from another machine does not carry that machine's id.
fn host_id(path: &Path) -> Result<String> {
    if let Ok(text) = std::fs::read_to_string(path)
        && let Some(id) = text.lines().next().filter(|id| id.starts_with("urn:uuid:"))
    {
        return Ok(id.to_owned());
    }
    let mut bytes = [0u8; 16];
    aws_lc_rs::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| Error::new("random_unavailable"))?;
    // UUID version 4, RFC 9562 variant.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let id = format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, format!("{id}\n"))?;
    Ok(id)
}

fn random(len: usize) -> Result<String> {
    let mut bytes = vec![0u8; len];
    aws_lc_rs::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| Error::new("random_unavailable"))?;
    Ok(B64.encode(bytes))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The system's opener, unless this shell came in over SSH, where the
/// browser is on the other machine; the printed address serves either way.
fn open_browser(url: &str) {
    if std::env::var_os("SSH_CONNECTION").is_some() {
        return;
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

async fn post_form(client: &reqwest::Client, url: &str, pairs: &[(&str, &str)]) -> Result<Value> {
    let response = client
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .body(login::form(pairs))
        .send()
        .await
        .map_err(|error| Error::with("login_failed", format!("{url}: {error}")))?;
    answer(url, response).await
}

async fn get_json(client: &reqwest::Client, url: &str) -> Result<Value> {
    let response = client
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|error| Error::with("login_failed", format!("{url}: {error}")))?;
    answer(url, response).await
}

async fn answer(url: &str, response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    // Stops reading past the limit rather than holding a whole answer.
    let body = login::bounded(response)
        .await
        .map_err(|error| Error::with("login_failed", format!("{url}: {}", error.code)))?;
    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    if !status.is_success() {
        let code = value["error"].as_str().unwrap_or("refused");
        let why = value["error_description"].as_str().unwrap_or_default();
        return fail_with(
            "login_failed",
            format!("{url}: HTTP {} {code} {why}", status.as_u16()).trim_end(),
        );
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::rsa::{KeyPair, KeySize};
    use aws_lc_rs::signature::KeyPair as _;

    #[tokio::test]
    async fn a_file_that_is_not_a_login_is_not_signed_in_over() {
        let dir = std::env::temp_dir().join(format!("agent-sign-in-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chatgpt.json");
        std::fs::write(&path, "{\"client_id\":").unwrap();
        let error = sign_in(&path, 1).await.unwrap_err();
        assert_eq!(error.code, "login_unreadable");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"client_id\":");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// `n` and `e` from a DER `RSAPublicKey` (RFC 8017), as a JWKS lists them.
    fn components(der: &[u8]) -> (Vec<u8>, Vec<u8>) {
        fn item(der: &[u8]) -> (&[u8], &[u8]) {
            let (len, at) = match der[1] {
                n if n < 0x80 => (n as usize, 2),
                n => {
                    let bytes = (n & 0x7f) as usize;
                    let len = der[2..2 + bytes]
                        .iter()
                        .fold(0, |len, b| len << 8 | *b as usize);
                    (len, 2 + bytes)
                }
            };
            (&der[at..at + len], &der[at + len..])
        }
        let (sequence, _) = item(der);
        let (n, rest) = item(sequence);
        let (e, _) = item(rest);
        let strip = |v: &[u8]| v.iter().skip_while(|b| **b == 0).copied().collect();
        (strip(n), strip(e))
    }

    fn jwt(key: &KeyPair, kid: &str, claims: &Value) -> String {
        let header = B64.encode(json!({"alg":"RS256","kid":kid}).to_string());
        let payload = B64.encode(claims.to_string());
        let signed = format!("{header}.{payload}");
        let mut signature = vec![0; key.public_modulus_len()];
        key.sign(
            &signature::RSA_PKCS1_SHA256,
            &aws_lc_rs::rand::SystemRandom::new(),
            signed.as_bytes(),
            &mut signature,
        )
        .unwrap();
        format!("{signed}.{}", B64.encode(signature))
    }

    #[test]
    fn an_id_token_is_accepted_only_from_the_issuer_for_this_client_and_sign_in() {
        let key = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let (n, e) = components(key.public_key().as_ref());
        let keys = json!({"keys":[{"kty":"RSA","kid":"k1","n":B64.encode(n),"e":B64.encode(e)}]});
        let claims = json!({"iss":"https://auth.openai.com","aud":["oaiapp_x"],"exp":2000,
            "nonce":"n","email":"someone@example.com","sub":"s"});
        let check = |token: &str| {
            validate_id_token(
                token,
                &keys,
                "https://auth.openai.com",
                "oaiapp_x",
                "n",
                1000,
            )
        };
        let accepted = check(&jwt(&key, "k1", &claims)).unwrap();
        assert_eq!(accepted["email"], "someone@example.com");
        let refused = |token: String| check(&token).unwrap_err().detail.unwrap();
        let with = |field: &str, value: Value| {
            let mut claims = claims.clone();
            claims[field] = value;
            jwt(&key, "k1", &claims)
        };
        assert!(refused(with("aud", json!("oaiapp_y"))).contains("another client"));
        assert!(refused(with("iss", json!("https://evil.example"))).contains("another issuer"));
        assert!(refused(with("exp", json!(999))).contains("expired"));
        assert!(refused(with("nonce", json!("m"))).contains("nonce"));
        assert!(refused(jwt(&key, "k2", &claims)).contains("does not list"));
        let other = KeyPair::generate(KeySize::Rsa2048).unwrap();
        assert!(refused(jwt(&other, "k1", &claims)).contains("signature"));
        let token = jwt(&key, "k1", &claims);
        let (signed, _) = token.rsplit_once('.').unwrap();
        let unsigned = format!(
            "{}.{}.",
            B64.encode(r#"{"alg":"none"}"#),
            signed.split('.').nth(1).unwrap()
        );
        assert!(refused(unsigned).contains("RS256"));
        assert!(refused("a.b".into()).contains("JWT"));
    }

    #[test]
    fn the_authorize_address_registers_once_then_signs_in_as_the_issued_client() {
        let first = Authorize {
            client_id: None,
            id_token_hint: None,
            host: "urn:uuid:00000000-0000-4000-8000-000000000000",
            redirect: "http://127.0.0.1:1455/auth/callback",
            state: "s",
            nonce: "n",
            challenge: "c",
        };
        let pairs = |url: reqwest::Url| -> Vec<(String, String)> {
            url.query_pairs()
                .map(|(k, v)| (k.into(), v.into()))
                .collect()
        };
        let query = pairs(authorize_url(&first));
        let get = |query: &[(String, String)], key: &str| {
            query.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
        };
        assert_eq!(get(&query, "client_id").as_deref(), Some(REGISTER));
        assert_eq!(get(&query, "agent_name_hint").as_deref(), Some(NAME));
        assert_eq!(get(&query, "scope").as_deref(), Some(SCOPE));
        assert_eq!(
            get(&query, "resource").as_deref(),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(
            get(&query, "code_challenge_method").as_deref(),
            Some("S256")
        );
        assert_eq!(
            get(&query, "redirect_uri").as_deref(),
            Some("http://127.0.0.1:1455/auth/callback")
        );
        let again = pairs(authorize_url(&Authorize {
            client_id: Some("oaiapp_x"),
            id_token_hint: Some("id"),
            ..first
        }));
        assert_eq!(get(&again, "client_id").as_deref(), Some("oaiapp_x"));
        assert_eq!(get(&again, "id_token_hint").as_deref(), Some("id"));
        assert_eq!(get(&again, "agent_name_hint"), None);
        assert!(login::has_scope(SCOPE, PLAN_SCOPE));
    }

    async fn call(
        listener: &tokio::net::TcpListener,
        target: &'static str,
        idle: bool,
    ) -> Result<Callback> {
        let address = listener.local_addr().unwrap();
        let browser = tokio::spawn(async move {
            // A preconnect that never sends a request can come first.
            let _idle = match idle {
                true => Some(tokio::net::TcpStream::connect(address).await.unwrap()),
                false => None,
            };
            for target in ["/favicon.ico", target] {
                let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
                socket
                    .write_all(format!("GET {target} HTTP/1.1\r\nhost: x\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
                let mut page = String::new();
                socket.read_to_string(&mut page).await.unwrap();
            }
        });
        let callback = receive(listener, "s").await;
        browser.await.unwrap();
        callback
    }

    #[tokio::test]
    async fn the_callback_must_carry_this_sign_ins_state_and_a_code() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        assert_eq!(
            call(
                &listener,
                "/auth/callback?code=c&state=s&scope=a+b&client_id=oaiapp_x",
                true,
            )
            .await
            .unwrap(),
            Callback {
                code: "c".into(),
                scope: Some("a b".into()),
                client_id: Some("oaiapp_x".into()),
            }
        );
        let wrong = call(&listener, "/auth/callback?code=c&state=t", false).await;
        assert!(wrong.unwrap_err().detail.unwrap().contains("state"));
        let denied = call(
            &listener,
            "/auth/callback?error=access_denied&state=s",
            false,
        )
        .await;
        assert_eq!(denied.unwrap_err().code, "login_denied");
        let empty = call(&listener, "/auth/callback?state=s", false).await;
        assert_eq!(empty.unwrap_err().code, "login_failed");
    }

    #[test]
    fn a_host_id_is_made_once_and_kept() {
        let dir = std::env::temp_dir().join(format!("agent-host-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("host-id");
        let id = host_id(&path).unwrap();
        assert!(id.starts_with("urn:uuid:") && id.len() == 45, "{id}");
        assert_eq!(&id[23..24], "4");
        assert_eq!(host_id(&path).unwrap(), id);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
