//! The network a sandboxed command reaches when its bot names domains, as
//! Codex's network proxy gives it: an HTTP proxy that lets a request
//! through to a host its `domains` allow and none deny, and answers any
//! other with `403` and `x-proxy-error`. The command's own network is off;
//! it reaches this proxy alone (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`),
//! by a descriptor on Linux ([`super::bridge`]) and by one loopback port
//! on macOS. Plain HTTP is forwarded and HTTPS tunneled (`CONNECT`), never
//! opened, as Codex's full mode does.
//!
//! Patterns are Codex's: an exact host; `*.example.com`, its subdomains;
//! `**.example.com`, the domain and its subdomains; `?` one byte and `*`
//! within a pattern any run of a label's bytes; no lone `*`. Any case.
//! A deny wins over every allow, and with no allow nothing goes. A local or
//! private address is reached only when its own literal (or `localhost`)
//! is allowed, and a name that resolves to one never is: the proxy
//! connects to the addresses it checked, so a name can't resolve again
//! elsewhere between the check and the connection.

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// What a domain pattern lets through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Allow,
    Deny,
}

/// The most patterns a bot names.
pub const DOMAINS: usize = 256;

/// The longest request head the proxy reads.
const HEAD_BYTES: usize = 64 * 1024;

/// Whether `pattern` is one a bot may name, or why not.
pub fn check(pattern: &str) -> std::result::Result<(), String> {
    let host = pattern
        .strip_prefix("**.")
        .or_else(|| pattern.strip_prefix("*."))
        .unwrap_or(pattern);
    if pattern.is_empty() || pattern.len() > 253 || host.is_empty() || host == "*" {
        return Err(format!(
            "{pattern:?} is a host, *.domain or **.domain, never every host"
        ));
    }
    let literal = host.starts_with('[') && host.ends_with(']');
    if !literal
        && !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-.*?_:".contains(&b))
    {
        return Err(format!(
            "{pattern:?} is a host name: letters, digits, - . _ ? *"
        ));
    }
    Ok(())
}

/// `PATTERN`, `PATTERN=allow` or `PATTERN=deny`, as a command line names
/// a domain.
pub fn flag(given: &str) -> std::result::Result<(String, Access), String> {
    let (pattern, access) = match given.rsplit_once('=') {
        Some((pattern, "allow")) => (pattern, Access::Allow),
        Some((pattern, "deny")) => (pattern, Access::Deny),
        Some(_) => {
            return Err(format!(
                "{given:?} is PATTERN, PATTERN=allow or PATTERN=deny"
            ));
        }
        None => (given, Access::Allow),
    };
    check(pattern)?;
    Ok((pattern.to_ascii_lowercase(), access))
}

/// Whether `host`, lowercase, matches `pattern`.
pub fn matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    if let Some(domain) = pattern.strip_prefix("**.") {
        return glob(domain.as_bytes(), host.as_bytes())
            || host
                .split_once('.')
                .is_some_and(|(_, rest)| suffix(domain, rest));
    }
    if let Some(domain) = pattern.strip_prefix("*.") {
        return host
            .split_once('.')
            .is_some_and(|(_, rest)| suffix(domain, rest));
    }
    glob(pattern.as_bytes(), host.as_bytes())
}

/// Whether `domain` matches `rest` or one of its parent domains.
fn suffix(domain: &str, rest: &str) -> bool {
    let mut rest = rest;
    loop {
        if glob(domain.as_bytes(), rest.as_bytes()) {
            return true;
        }
        match rest.split_once('.') {
            Some((_, parent)) => rest = parent,
            None => return false,
        }
    }
}

/// `?` one byte, `*` any run of bytes but a dot; the rest itself. One
/// pass of the pattern over a row per text position: time pattern times
/// text, whatever the pattern, never backtracking.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    // `ends[n]`: the pattern so far matches the text's first `n` bytes.
    let mut ends = vec![false; text.len() + 1];
    ends[0] = true;
    for &byte in pattern {
        let mut next = vec![false; text.len() + 1];
        match byte {
            b'*' => {
                for n in 0..=text.len() {
                    next[n] = ends[n] || (n > 0 && next[n - 1] && text[n - 1] != b'.');
                }
            }
            _ => {
                for n in 1..=text.len() {
                    next[n] = ends[n - 1] && (byte == b'?' || text[n - 1] == byte);
                }
            }
        }
        ends = next;
    }
    ends[text.len()]
}

/// Why a host is not reached, as the proxy's `x-proxy-error` says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    Denied,
    NotAllowed,
    Local,
}

impl Blocked {
    /// Why `host` is not reached, in words.
    pub fn why(self, host: &str) -> String {
        match self {
            Blocked::Denied => format!("{host} is among the domains this agent may not reach"),
            Blocked::NotAllowed => format!("{host} is not among the domains this agent reaches"),
            Blocked::Local => {
                format!("{host} is on a local network, which it reaches only by an allowed address")
            }
        }
    }

    fn header(self) -> &'static str {
        match self {
            Blocked::Denied => "blocked-by-denylist",
            Blocked::NotAllowed => "blocked-by-allowlist",
            Blocked::Local => "blocked-by-local-network",
        }
    }
}

/// What `domains` make of `host`, lowercase, before it is resolved.
pub fn decide(
    domains: &std::collections::BTreeMap<String, Access>,
    host: &str,
) -> Result<(), Blocked> {
    let hit = |access| {
        (domains.iter())
            .filter(|(_, given)| **given == access)
            .any(|(pattern, _)| matches(pattern, host))
    };
    if hit(Access::Deny) {
        return Err(Blocked::Denied);
    }
    if !hit(Access::Allow) {
        return Err(Blocked::NotAllowed);
    }
    Ok(())
}

/// What `domains` make of `host`, as a URL names it: `[::1]` is named
/// either way.
pub fn allowed(
    domains: &std::collections::BTreeMap<String, Access>,
    host: &str,
) -> Result<(), Blocked> {
    let host = host.to_ascii_lowercase();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let (named, other) = (decide(domains, &host), decide(domains, bare));
    // Both spellings name one place: a deny of either wins, and an allow
    // of either lets it through.
    match (named, other) {
        (Err(Blocked::Denied), _) | (_, Err(Blocked::Denied)) => Err(Blocked::Denied),
        (Ok(()), _) | (_, Ok(())) => Ok(()),
        (Err(blocked), _) => Err(blocked),
    }
}

/// Whether `ip` is on this machine or a private network.
fn local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || (a == 100 && (64..128).contains(&b))
                || a == 0
        }
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(v4) => local(IpAddr::V4(v4)),
            None => {
                let first = ip.segments()[0];
                ip.is_loopback()
                    || ip.is_unspecified()
                    || ip.is_multicast()
                    || (first & 0xfe00) == 0xfc00
                    || (first & 0xffc0) == 0xfe80
            }
        },
    }
}

/// The addresses to connect to for `host:port`, once `domains` let it:
/// a local one only when its literal, or `localhost`, is allowed by name.
async fn reach(
    domains: &std::collections::BTreeMap<String, Access>,
    host: &str,
    port: u16,
) -> Result<Vec<SocketAddr>, Blocked> {
    let host = host.to_ascii_lowercase();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    allowed(domains, &host)?;
    if let Ok(ip) = bare.parse::<IpAddr>() {
        // A literal, allowed by its own name.
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let found: Vec<SocketAddr> = tokio::net::lookup_host((bare, port))
        .await
        .map(Iterator::collect)
        .unwrap_or_default();
    let named_local = bare == "localhost";
    if found.iter().any(|addr| local(addr.ip())) && !named_local {
        return Err(Blocked::Local);
    }
    Ok(found)
}

/// Answer one client of the proxy on `stream` under `domains`: one
/// request forwarded, or one tunnel, then the two ends joined until either
/// closes.
pub async fn serve<S>(
    stream: S,
    domains: std::sync::Arc<std::collections::BTreeMap<String, Access>>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut client = BufReader::new(stream);
    let Some(head) = read_head(&mut client).await else {
        return;
    };
    let mut lines = head.split("\r\n");
    let request = lines.next().unwrap_or_default();
    let mut words = request.split(' ');
    let (Some(method), Some(target), Some(version)) = (words.next(), words.next(), words.next())
    else {
        let _ = refuse(
            client.get_mut(),
            "400 Bad Request",
            None,
            "not an HTTP request",
        )
        .await;
        return;
    };
    let connect = method.eq_ignore_ascii_case("CONNECT");
    let (authority, path) = match connect {
        true => (target, ""),
        false => match target.strip_prefix("http://") {
            Some(rest) => rest.split_at(rest.find('/').unwrap_or(rest.len())),
            None => {
                let _ = refuse(
                    client.get_mut(),
                    "400 Bad Request",
                    None,
                    "the proxy forwards http:// requests and tunnels the rest with CONNECT",
                )
                .await;
                return;
            }
        },
    };
    let Some((host, port)) = split_authority(authority, if connect { 443 } else { 80 }) else {
        let _ = refuse(
            client.get_mut(),
            "400 Bad Request",
            None,
            "no host to reach",
        )
        .await;
        return;
    };
    let addresses = match reach(&domains, &host, port).await {
        Ok(addresses) => addresses,
        Err(blocked) => {
            let why = blocked.why(&host);
            let _ = refuse(client.get_mut(), "403 Forbidden", Some(blocked), &why).await;
            return;
        }
    };
    let Ok(mut upstream) = tokio::net::TcpStream::connect(&addresses[..]).await else {
        let _ = refuse(
            client.get_mut(),
            "502 Bad Gateway",
            None,
            &format!("{host} did not answer"),
        )
        .await;
        return;
    };
    if connect {
        if client
            .get_mut()
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }
    } else {
        // In origin form, without what is the proxy's, one request alone:
        // a next one on the connection may be for another host.
        let path = if path.is_empty() { "/" } else { path };
        let mut sent = format!("{method} {path} {version}\r\n");
        for line in lines.filter(|line| !line.is_empty()) {
            let name = line.split(':').next().unwrap_or_default().trim();
            if [
                "proxy-connection",
                "proxy-authorization",
                "connection",
                "keep-alive",
            ]
            .iter()
            .any(|own| name.eq_ignore_ascii_case(own))
            {
                continue;
            }
            sent.push_str(line);
            sent.push_str("\r\n");
        }
        sent.push_str("Connection: close\r\n\r\n");
        if upstream.write_all(sent.as_bytes()).await.is_err() {
            return;
        }
    }
    // What the client sent past its head is the body, or the tunnel's start.
    let early = client.buffer().to_vec();
    if !early.is_empty() && upstream.write_all(&early).await.is_err() {
        return;
    }
    let mut client = client.into_inner();
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}

/// The request's head up to its blank line, or none past [`HEAD_BYTES`].
async fn read_head<S: AsyncRead + Unpin>(client: &mut BufReader<S>) -> Option<String> {
    let mut head = Vec::new();
    loop {
        // Never more than the head may hold, a line without its end too.
        let left = (HEAD_BYTES + 1).saturating_sub(head.len()) as u64;
        let read = (&mut *client)
            .take(left)
            .read_until(b'\n', &mut head)
            .await
            .ok()?;
        if read == 0 || head.len() > HEAD_BYTES {
            return None;
        }
        if head.ends_with(b"\r\n\r\n") || head == b"\r\n" {
            break;
        }
    }
    let head = String::from_utf8(head).ok()?;
    Some(head.trim_end_matches("\r\n").to_owned())
}

/// `host:port`, `host` or `[v6]:port`, lowercase.
fn split_authority(authority: &str, default: u16) -> Option<(String, u16)> {
    let authority = authority.rsplit_once('@').map_or(authority, |(_, at)| at);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port))
            if !port.contains(']') && (!host.contains(':') || host.ends_with(']')) =>
        {
            (host, port.parse().ok()?)
        }
        _ => (authority, default),
    };
    (!host.is_empty()).then(|| (host.to_ascii_lowercase(), port))
}

async fn refuse<S: AsyncWrite + Unpin>(
    client: &mut S,
    status: &str,
    blocked: Option<Blocked>,
    why: &str,
) -> std::io::Result<()> {
    let header = blocked.map_or(String::new(), |blocked| {
        format!("x-proxy-error: {}\r\n", blocked.header())
    });
    let body = format!("agent: {why}\n");
    let answer = format!(
        "HTTP/1.1 {status}\r\n{header}Content-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(answer.as_bytes()).await?;
    client.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn domains(given: &[(&str, Access)]) -> BTreeMap<String, Access> {
        given.iter().map(|(p, a)| (p.to_string(), *a)).collect()
    }

    #[test]
    fn patterns_match_as_codex_s_do() {
        assert!(matches("github.com", "github.com"));
        assert!(!matches("github.com", "api.github.com"));
        assert!(matches("*.github.com", "api.github.com"));
        assert!(matches("*.github.com", "a.b.github.com"));
        assert!(!matches("*.github.com", "github.com"));
        assert!(matches("**.github.com", "github.com"));
        assert!(matches("**.github.com", "api.github.com"));
        assert!(!matches("**.github.com", "evilgithub.com"));
        assert!(matches("api?.example.com", "api1.example.com"));
        assert!(!matches("api?.example.com", "api.example.com"));
        assert!(!matches("api?.example.com", "api12.example.com"));
        assert!(matches("GitHub.COM", "github.com"));
        assert!(matches("api-*.example.com", "api-eu.example.com"));
        assert!(!matches("api-*.example.com", "api-eu.x.example.com"));
        for bad in ["*", "", "*.", "**.", "a b.com", "a/b"] {
            assert!(check(bad).is_err(), "{bad}");
        }
        for good in [
            "github.com",
            "*.github.com",
            "**.github.com",
            "127.0.0.1",
            "[::1]",
            "localhost",
        ] {
            assert!(check(good).is_ok(), "{good}");
        }
    }

    #[test]
    fn a_deny_wins_and_no_allow_lets_nothing_through() {
        let given = domains(&[
            ("**.example.com", Access::Allow),
            ("evil.example.com", Access::Deny),
        ]);
        assert_eq!(decide(&given, "api.example.com"), Ok(()));
        assert_eq!(decide(&given, "evil.example.com"), Err(Blocked::Denied));
        assert_eq!(decide(&given, "other.com"), Err(Blocked::NotAllowed));
        assert_eq!(
            decide(&domains(&[("x.com", Access::Deny)]), "y.com"),
            Err(Blocked::NotAllowed)
        );
    }

    #[test]
    fn a_pattern_of_many_stars_matches_in_time_bounded_by_its_length() {
        let pattern = "*a*a*a*a*a*a*a*a*a*a*a*a*ab.example.com";
        let host = format!("{}.example.com", "a".repeat(60));
        let started = std::time::Instant::now();
        assert!(!matches(pattern, &host));
        assert!(matches(
            pattern,
            &format!("{}b.example.com", "a".repeat(60))
        ));
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }

    #[test]
    fn a_deny_of_either_spelling_of_a_v6_address_wins() {
        let bare_allowed = domains(&[("::1", Access::Allow), ("[::1]", Access::Deny)]);
        assert_eq!(allowed(&bare_allowed, "[::1]"), Err(Blocked::Denied));
        let bracket_allowed = domains(&[("[::1]", Access::Allow), ("::1", Access::Deny)]);
        assert_eq!(allowed(&bracket_allowed, "[::1]"), Err(Blocked::Denied));
        assert_eq!(
            allowed(&domains(&[("::1", Access::Allow)]), "[::1]"),
            Ok(())
        );
        assert_eq!(
            allowed(&domains(&[("[::1]", Access::Allow)]), "[::1]"),
            Ok(())
        );
    }

    #[tokio::test]
    async fn a_head_without_a_line_end_is_never_read_past_its_bound() {
        let (mut sender, receiver) = tokio::io::duplex(1 << 20);
        // A line with no end, longer than any head, and the sender still open.
        sender
            .write_all(&vec![b'x'; HEAD_BYTES + 4096])
            .await
            .unwrap();
        let mut client = BufReader::new(receiver);
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), read_head(&mut client));
        assert_eq!(read.await.unwrap(), None);
        drop(sender);
    }

    #[tokio::test]
    async fn a_local_address_is_reached_only_by_its_own_allowed_name() {
        let every = domains(&[
            ("**.localhost", Access::Allow),
            ("127.0.0.1", Access::Allow),
        ]);
        assert_eq!(
            reach(&every, "127.0.0.1", 80).await,
            Ok(vec!["127.0.0.1:80".parse().unwrap()])
        );
        assert_eq!(
            reach(&every, "10.0.0.1", 80).await,
            Err(Blocked::NotAllowed)
        );
        let named = domains(&[("localhost", Access::Allow)]);
        assert!(reach(&named, "localhost", 80).await.is_ok());
        let v6 = domains(&[("::1", Access::Allow)]);
        assert_eq!(
            reach(&v6, "[::1]", 80).await,
            Ok(vec!["[::1]:80".parse().unwrap()])
        );
        assert!(local("192.168.1.2".parse().unwrap()));
        assert!(local("100.100.1.1".parse().unwrap()));
        assert!(local("fd00::1".parse().unwrap()));
        assert!(local("::ffff:10.0.0.1".parse().unwrap()));
        assert!(!local("8.8.8.8".parse().unwrap()));
        assert!(!local("2606:4700::1111".parse().unwrap()));
    }

    #[test]
    fn an_authority_is_split_into_its_host_and_port() {
        assert_eq!(
            split_authority("Example.com:8443", 443),
            Some(("example.com".into(), 8443))
        );
        assert_eq!(
            split_authority("example.com", 80),
            Some(("example.com".into(), 80))
        );
        assert_eq!(
            split_authority("[::1]:8080", 80),
            Some(("[::1]".into(), 8080))
        );
        assert_eq!(split_authority("[::1]", 80), Some(("[::1]".into(), 80)));
        assert_eq!(
            split_authority("user:pw@host:81", 80),
            Some(("host".into(), 81))
        );
        assert_eq!(split_authority(":80", 80), None);
    }

    /// One request through the proxy to a server on this machine, allowed
    /// by its literal; another host refused with the proxy's reason.
    #[tokio::test]
    async fn a_request_goes_to_an_allowed_host_and_another_is_refused() {
        use tokio::io::AsyncReadExt;
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = server.accept().await.unwrap();
            let mut seen = vec![0; 4096];
            let n = stream.read(&mut seen).await.unwrap();
            let seen = String::from_utf8_lossy(&seen[..n]).to_string();
            let body = seen.lines().next().unwrap_or_default().to_owned();
            let answer = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(answer.as_bytes()).await.unwrap();
        });
        let allowed = std::sync::Arc::new(domains(&[("127.0.0.1", Access::Allow)]));
        let ask = |request: String| {
            let allowed = allowed.clone();
            async move {
                let (mut ours, theirs) = tokio::io::duplex(1 << 16);
                tokio::spawn(serve(theirs, allowed));
                ours.write_all(request.as_bytes()).await.unwrap();
                let mut answer = String::new();
                ours.read_to_string(&mut answer).await.unwrap();
                answer
            }
        };
        let answer = ask(format!(
            "GET http://127.0.0.1:{port}/a?b HTTP/1.1\r\nHost: 127.0.0.1\r\nProxy-Connection: keep-alive\r\n\r\n"
        ))
        .await;
        assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
        assert!(answer.ends_with("GET /a?b HTTP/1.1"), "{answer}");
        let refused =
            ask("CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n".into()).await;
        assert!(refused.starts_with("HTTP/1.1 403 Forbidden"), "{refused}");
        assert!(
            refused.contains("x-proxy-error: blocked-by-allowlist"),
            "{refused}"
        );
        let https = ask("GET https://example.com/ HTTP/1.1\r\n\r\n".into()).await;
        assert!(https.starts_with("HTTP/1.1 400"), "{https}");
    }
}
