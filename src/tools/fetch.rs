//! `web_fetch`: a public page as text, fetched by the daemon, so a bot on
//! any provider can read a URL it was given. HTML comes back as Markdown,
//! as fx's and Claude Code's fetch tools return it; other text as it is.
use crate::{Error, Result, fail_with};
use reqwest::Url;
use std::{sync::OnceLock, time::Duration};

/// Body bytes read from the server; a longer page is cut there.
const BODY_BYTES: usize = 4 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

/// The page at `url`: its final address, status, type, title and text.
pub struct Page {
    pub url: String,
    pub status: u16,
    pub content_type: String,
    pub title: Option<String>,
    pub text: String,
    /// The body passed `BODY_BYTES` and was cut there.
    pub cut: bool,
}

/// A URL the tool may fetch: absolute, http or https.
pub fn parse(url: &str) -> Result<Url> {
    let parsed = Url::parse(url).map_err(|e| Error::with("invalid_url", e.to_string()))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return fail_with("invalid_url", "only http and https URLs are fetched");
    }
    Ok(parsed)
}

fn client() -> Result<&'static reqwest::Client> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let client = reqwest::Client::builder()
        .user_agent(concat!("agent-runtime/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::limited(10))
        .connect_timeout(Duration::from_secs(10))
        .timeout(TIMEOUT)
        .pool_max_idle_per_host(4)
        .build()
        .map_err(|_| Error::new("http_client_init"))?;
    Ok(CLIENT.get_or_init(|| client))
}

pub async fn fetch(url: Url) -> Result<Page> {
    let mut response = client()?
        .get(url)
        .header(
            reqwest::header::ACCEPT,
            "text/html, text/markdown, text/plain, application/json;q=0.9, */*;q=0.5",
        )
        .send()
        .await
        .map_err(|e| Error::with("web_fetch_failed", without_url(e)))?;
    let status = response.status().as_u16();
    let url = response.url().to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let textual = mime.is_empty()
        || mime.starts_with("text/")
        || mime.ends_with("+xml")
        || mime.ends_with("+json")
        || matches!(
            mime.as_str(),
            "application/json" | "application/xml" | "application/javascript"
        );
    if !textual {
        return fail_with(
            "web_fetch_not_text",
            format!("{mime} from {url}; web_fetch returns text"),
        );
    }
    let mut body = Vec::new();
    let mut cut = false;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| Error::with("web_fetch_failed", without_url(e)))?
    {
        let room = BODY_BYTES - body.len();
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            cut = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }
    let body = String::from_utf8_lossy(&body);
    let html = mime == "text/html"
        || mime == "application/xhtml+xml"
        || (mime.is_empty() && looks_like_html(&body));
    let (title, text) = if html {
        let base = Url::parse(&url).ok();
        markdown(&body, base.as_ref())
    } else {
        (None, body.into_owned())
    };
    Ok(Page {
        url,
        status,
        content_type,
        title,
        text,
        cut,
    })
}

/// reqwest's message, which repeats the URL the model already has.
fn without_url(error: reqwest::Error) -> String {
    error.without_url().to_string()
}

fn looks_like_html(body: &str) -> bool {
    let start = body.trim_start().get(..64).unwrap_or(body.trim_start());
    let start = start.to_ascii_lowercase();
    start.starts_with("<!doctype html") || start.starts_with("<html")
}

/// Elements whose content is never shown.
const HIDDEN: [&str; 8] = [
    "script", "style", "noscript", "svg", "template", "iframe", "object", "canvas",
];
/// Elements that start and end a block of text.
const BLOCKS: [&str; 22] = [
    "p",
    "div",
    "section",
    "article",
    "header",
    "footer",
    "main",
    "nav",
    "aside",
    "form",
    "figure",
    "figcaption",
    "table",
    "thead",
    "tbody",
    "ul",
    "ol",
    "dl",
    "dt",
    "dd",
    "details",
    "summary",
];

/// An HTML page's title and its text as Markdown: headings, links, lists,
/// code, quotes and tables kept as Markdown writes them; scripts, styles
/// and markup dropped. `base` resolves relative links.
pub fn markdown(html: &str, base: Option<&Url>) -> (Option<String>, String) {
    let mut out = Markdown::default();
    let mut title = None;
    let mut rest = html;
    while let Some(at) = rest.find('<') {
        out.text(&rest[..at]);
        rest = &rest[at..];
        if let Some(comment) = rest.strip_prefix("<!--") {
            rest = comment.find("-->").map_or("", |end| &comment[end + 3..]);
            continue;
        }
        let Some(end) = tag_end(rest) else {
            out.text(rest);
            rest = "";
            break;
        };
        let tag = &rest[1..end];
        rest = &rest[end + 1..];
        let closing = tag.starts_with('/');
        let tag = tag.trim_start_matches('/');
        let name_end = tag
            .find(|c: char| c.is_ascii_whitespace() || c == '/')
            .unwrap_or(tag.len());
        let name = tag[..name_end].to_ascii_lowercase();
        if name.is_empty() || name.starts_with('!') || name.starts_with('?') {
            continue;
        }
        if !closing && (HIDDEN.contains(&name.as_str()) || name == "title") {
            // Skip to the matching close tag; its content is not markup.
            let lower = rest.to_ascii_lowercase();
            let close = format!("</{name}");
            let content_end = lower.find(&close).unwrap_or(rest.len());
            if name == "title" && title.is_none() {
                let text = collapse(&decode(&rest[..content_end]));
                if !text.is_empty() {
                    title = Some(text);
                }
            }
            rest = &rest[content_end..];
            rest = rest.find('>').map_or("", |end| &rest[end + 1..]);
            continue;
        }
        out.tag(&name, closing, tag, base);
    }
    out.text(rest);
    (title, out.finish())
}

/// The `>` that ends the tag at the start of `text`, past quoted values.
fn tag_end(text: &str) -> Option<usize> {
    let mut quote = None;
    for (index, c) in text.char_indices().skip(1) {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return Some(index),
            _ => {}
        }
    }
    None
}

/// The value of attribute `name` in a tag's text.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find(name) {
        let at = from + found;
        from = at + name.len();
        let before = lower[..at].chars().next_back();
        if !before.is_some_and(|c| c.is_ascii_whitespace()) {
            continue;
        }
        let after = tag[from..].trim_start();
        let Some(value) = after.strip_prefix('=') else {
            continue;
        };
        let value = value.trim_start();
        let raw = match value.chars().next() {
            Some(q @ ('"' | '\'')) => value[1..].split(q).next().unwrap_or(""),
            _ => value
                .split(|c: char| c.is_ascii_whitespace() || c == '>')
                .next()
                .unwrap_or(""),
        };
        return Some(decode(raw));
    }
    None
}

/// Replace character references with the characters they name.
fn decode(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let end = rest[1..]
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '#')
            .map(|end| end + 1);
        let decoded = end
            .filter(|end| rest[*end..].starts_with(';'))
            .and_then(|end| {
                let name = &rest[1..end];
                let c = match name {
                    "amp" => '&',
                    "lt" => '<',
                    "gt" => '>',
                    "quot" => '"',
                    "apos" => '\'',
                    "nbsp" => ' ',
                    "mdash" => '—',
                    "ndash" => '–',
                    "hellip" => '…',
                    "copy" => '©',
                    "rsquo" => '’',
                    "lsquo" => '‘',
                    "rdquo" => '”',
                    "ldquo" => '“',
                    _ => {
                        let number = name.strip_prefix('#')?;
                        let code = match number.strip_prefix(['x', 'X']) {
                            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                            None => number.parse().ok()?,
                        };
                        char::from_u32(code)?
                    }
                };
                Some((c, end + 1))
            });
        match decoded {
            Some((c, length)) => {
                out.push(c);
                rest = &rest[length..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Default)]
struct Markdown {
    out: String,
    /// Inside `pre`, where whitespace is kept.
    pre: usize,
    /// Open lists: `None` for bullets, else the next number.
    lists: Vec<Option<u32>>,
    /// Open links' targets, `None` for one shown as plain text.
    links: Vec<Option<String>>,
    /// The text since the last tag ended in whitespace.
    space: bool,
}

impl Markdown {
    fn text(&mut self, raw: &str) {
        if raw.is_empty() {
            return;
        }
        let text = decode(raw);
        if self.pre > 0 {
            self.out.push_str(&text);
            return;
        }
        let starts = text.starts_with(|c: char| c.is_whitespace());
        let ends = text.ends_with(|c: char| c.is_whitespace());
        let words = collapse(&text);
        if words.is_empty() {
            self.space |= starts;
            return;
        }
        if (starts || self.space) && !self.at_line_start() {
            self.out.push(' ');
        }
        self.out.push_str(&words);
        self.space = ends;
    }

    fn at_line_start(&self) -> bool {
        self.out.is_empty() || self.out.ends_with('\n') || self.out.ends_with(' ')
    }

    /// End the current line, and with `blank` leave an empty one after it.
    fn line(&mut self, blank: bool) {
        self.space = false;
        if self.out.is_empty() {
            return;
        }
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        if !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        if blank && !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
    }

    fn inline(&mut self, mark: &str) {
        if self.space && !self.at_line_start() {
            self.out.push(' ');
        }
        self.space = false;
        self.out.push_str(mark);
    }

    fn tag(&mut self, name: &str, closing: bool, tag: &str, base: Option<&Url>) {
        match (name, closing) {
            ("br", _) => {
                if self.pre > 0 {
                    self.out.push('\n');
                } else {
                    self.line(false);
                }
            }
            ("hr", false) => {
                self.line(true);
                self.out.push_str("---\n\n");
            }
            ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", false) => {
                self.line(true);
                let level = name[1..].parse().unwrap_or(1);
                self.out.push_str(&"#".repeat(level));
                self.out.push(' ');
            }
            ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", true) => self.line(true),
            ("pre", false) => {
                self.line(true);
                self.out.push_str("```\n");
                self.pre += 1;
            }
            ("pre", true) => {
                self.pre = self.pre.saturating_sub(1);
                if !self.out.ends_with('\n') {
                    self.out.push('\n');
                }
                self.out.push_str("```\n\n");
            }
            ("code" | "kbd" | "samp", _) if self.pre == 0 => {
                if closing {
                    self.out.push('`');
                } else {
                    self.inline("`");
                }
            }
            ("strong" | "b", _) if self.pre == 0 => {
                if closing {
                    self.out.push_str("**");
                } else {
                    self.inline("**");
                }
            }
            ("em" | "i", _) if self.pre == 0 => {
                if closing {
                    self.out.push('_');
                } else {
                    self.inline("_");
                }
            }
            ("blockquote", false) => {
                self.line(true);
                self.out.push_str("> ");
            }
            ("blockquote", true) => self.line(true),
            ("ul", false) => {
                self.line(self.lists.is_empty());
                self.lists.push(None);
            }
            ("ol", false) => {
                self.line(self.lists.is_empty());
                let start = attribute(tag, "start").and_then(|s| s.parse().ok());
                self.lists.push(Some(start.unwrap_or(1)));
            }
            ("ul" | "ol", true) => {
                self.lists.pop();
                self.line(self.lists.is_empty());
            }
            ("li", false) => {
                self.line(false);
                let depth = self.lists.len().saturating_sub(1);
                self.out.push_str(&"  ".repeat(depth));
                match self.lists.last_mut() {
                    Some(Some(number)) => {
                        self.out.push_str(&format!("{number}. "));
                        *number += 1;
                    }
                    _ => self.out.push_str("- "),
                }
            }
            ("li", true) => self.line(false),
            ("tr", _) => self.line(false),
            ("td" | "th", false) => {
                if self.out.is_empty() || self.out.ends_with('\n') {
                    self.out.push('|');
                }
                self.out.push(' ');
                self.space = false;
            }
            ("td" | "th", true) => self.out.push_str(" |"),
            ("a", false) => {
                let href = attribute(tag, "href").and_then(|href| link(&href, base));
                if href.is_some() {
                    self.inline("[");
                }
                self.links.push(href);
            }
            ("a", true) => {
                if let Some(Some(href)) = self.links.pop() {
                    if self.out.ends_with('[') {
                        self.out.pop();
                    } else {
                        self.out.push_str(&format!("]({href})"));
                    }
                }
            }
            ("img", false) => {
                let alt = attribute(tag, "alt").map(|alt| collapse(&alt));
                let src = attribute(tag, "src").and_then(|src| link(&src, base));
                if let (Some(alt), Some(src)) = (alt.filter(|alt| !alt.is_empty()), src) {
                    self.inline(&format!("![{alt}]({src})"));
                }
            }
            (name, _) if BLOCKS.contains(&name) => self.line(true),
            _ => {}
        }
    }

    fn finish(mut self) -> String {
        self.line(false);
        let mut out = String::with_capacity(self.out.len());
        let mut blank = 0;
        for line in self.out.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                blank += 1;
                if blank > 1 || out.is_empty() {
                    continue;
                }
            } else {
                blank = 0;
            }
            out.push_str(line);
            out.push('\n');
        }
        while out.ends_with("\n\n") {
            out.pop();
        }
        out
    }
}

/// A link's target as the page means it, or `None` for one that leads
/// nowhere a fetch could follow.
fn link(href: &str, base: Option<&Url>) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
        return None;
    }
    match base {
        Some(base) => base.join(href).ok().map(String::from),
        None => Some(href.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_reads_as_markdown() {
        let base = Url::parse("https://example.com/docs/page").unwrap();
        let (title, text) = markdown(
            r#"<!doctype html><html><head><title>The &amp; Page</title>
            <style>body { color: red }</style><script>let x = "<p>";</script></head>
            <body><nav><a href="/">Home</a></nav>
            <h1>Heading <em>one</em></h1>
            <p>Some   text with a <a href="other#part">relative link</a>,
            <code>code</code> and &lt;tags&gt;&#33;</p>
            <ul><li>first</li><li>second<ol start="3"><li>nested</li></ol></li></ul>
            <pre><code>fn main() {
    println!("hi");
}</code></pre>
            <table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table>
            <img src="pic.png" alt="A picture"><a href="javascript:void(0)">no link</a>
            <!-- hidden <p>comment</p> --></body></html>"#,
            Some(&base),
        );
        assert_eq!(title.as_deref(), Some("The & Page"));
        assert_eq!(
            text,
            "[Home](https://example.com/)\n\n\
             # Heading _one_\n\n\
             Some text with a [relative link](https://example.com/docs/other#part), `code` and <tags>!\n\n\
             - first\n\
             - second\n  \
             3. nested\n\n\
             ```\nfn main() {\n    println!(\"hi\");\n}\n```\n\n\
             | a | b |\n\
             | 1 | 2 |\n\n\
             ![A picture](https://example.com/docs/pic.png)no link\n"
        );
    }

    #[test]
    fn broken_markup_keeps_its_text() {
        let (title, text) = markdown("plain <b>bold</b> & more <unclosed", None);
        assert_eq!(title, None);
        assert_eq!(text, "plain **bold** & more <unclosed\n");
        assert_eq!(decode("&#x1F980; &bogus; &#xZZ; &"), "🦀 &bogus; &#xZZ; &");
        assert_eq!(
            attribute(r#"a data-href="x" href='y'"#, "href").as_deref(),
            Some("y")
        );
    }

    #[test]
    fn only_http_urls_are_fetched() {
        assert!(parse("https://example.com/").is_ok());
        assert_eq!(parse("file:///etc/passwd").unwrap_err().code, "invalid_url");
        assert_eq!(parse("example.com").unwrap_err().code, "invalid_url");
    }
}
