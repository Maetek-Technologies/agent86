//! `web_fetch`, ported from Python's `tools/builtin/web.py`.
//!
//! Fetching a model-chosen URL from the user's machine is a server-side request
//! forgery primitive: the harness sits inside the trust boundary, so cloud
//! metadata (`169.254.169.254`), the user's own services on `localhost`, and
//! private networks are all reachable unless something stops them. Every hop
//! (the URL and each redirect) is resolved and every address vetted before a
//! connection is made, the body is capped while streaming, and only textual
//! content types are read.

use super::{failure, success, Arguments, Tool, ToolContext, ToolError};
use crate::types::ToolResult;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::LazyLock;
use std::time::Duration;

/// Bytes read off the wire at most, applied before decoding.
pub const MAX_BODY_BYTES: usize = 2_000_000;
/// Redirects are followed manually so every hop is re-vetted; this bounds the chain.
pub const MAX_REDIRECTS: usize = 5;

const ALLOWED_CONTENT_TYPES: &[&str] = &[
    "text/",
    "application/json",
    "application/xml",
    "application/xhtml",
    "application/javascript",
    "application/ld+json",
];
const ALLOWED_CONTENT_TYPE_SUFFIXES: &[&str] = &["+json", "+xml"];
const ALLOW_HINT: &str =
    "Set [tools] web_allow_private = true (or tools.web_allow_private in config) \
     to allow private/loopback targets.";

/// `web_fetch` settings from `[tools]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSettings {
    pub allow_private: bool,
    pub user_agent: String,
    /// Characters of page text returned at most; `0` disables the cap.
    pub max_chars: usize,
}

impl Default for WebSettings {
    fn default() -> Self {
        Self {
            allow_private: false,
            user_agent: default_user_agent(),
            max_chars: 8000,
        }
    }
}

/// A descriptive User-Agent with a real contact URL; sites such as Wikipedia
/// refuse generic or browser-spoofed ones.
pub fn default_user_agent() -> String {
    format!(
        "agent86/{} (+https://github.com/Maetek-Technologies/agent86)",
        env!("CARGO_PKG_VERSION")
    )
}

// --------------------------------------------------------------------------
// Address vetting, with the semantics of Python 3.12's `ipaddress`.
// --------------------------------------------------------------------------

/// Reduce tunnelled or mapped IPv6 forms to the IPv4 address they reach.
fn unwrap(ip: IpAddr) -> IpAddr {
    let IpAddr::V6(v6) = ip else {
        return ip;
    };
    let bits = u128::from(v6);
    if bits >> 32 == 0xffff {
        return IpAddr::V4(Ipv4Addr::from((bits & 0xffff_ffff) as u32));
    }
    if bits >> 112 == 0x2002 {
        return IpAddr::V4(Ipv4Addr::from(((bits >> 80) & 0xffff_ffff) as u32));
    }
    if bits >> 96 == 0x2001_0000 {
        return IpAddr::V4(Ipv4Addr::from(!(bits as u32)));
    }
    ip
}

fn v4_in(ip: Ipv4Addr, network: [u8; 4], prefix: u32) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    u32::from(ip) & mask == u32::from(Ipv4Addr::from(network)) & mask
}

fn v6_in(ip: Ipv6Addr, network: u128, prefix: u32) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    };
    u128::from(ip) & mask == network & mask
}

fn v4_private(ip: Ipv4Addr) -> bool {
    if ip == Ipv4Addr::new(192, 0, 0, 9) || ip == Ipv4Addr::new(192, 0, 0, 10) {
        return false;
    }
    [
        ([0, 0, 0, 0], 8),
        ([10, 0, 0, 0], 8),
        ([127, 0, 0, 0], 8),
        ([169, 254, 0, 0], 16),
        ([172, 16, 0, 0], 12),
        ([192, 0, 0, 0], 24),
        ([192, 0, 2, 0], 24),
        ([192, 168, 0, 0], 16),
        ([198, 18, 0, 0], 15),
        ([198, 51, 100, 0], 24),
        ([203, 0, 113, 0], 24),
        ([240, 0, 0, 0], 4),
        ([255, 255, 255, 255], 32),
    ]
    .iter()
    .any(|(network, prefix)| v4_in(ip, *network, *prefix))
}

fn v6_private(ip: Ipv6Addr) -> bool {
    let excepted = [
        (0x2001_0001_0000_0000_0000_0000_0000_0001, 128),
        (0x2001_0001_0000_0000_0000_0000_0000_0002, 128),
        (0x2001_0003_0000_0000_0000_0000_0000_0000, 32),
        (0x2001_0004_0112_0000_0000_0000_0000_0000, 48),
        (0x2001_0020_0000_0000_0000_0000_0000_0000, 28),
        (0x2001_0030_0000_0000_0000_0000_0000_0000, 28),
    ];
    if excepted
        .iter()
        .any(|(network, prefix)| v6_in(ip, *network, *prefix))
    {
        return false;
    }
    [
        (0x0000_0000_0000_0000_0000_0000_0000_0001, 128),
        (0x0000_0000_0000_0000_0000_0000_0000_0000, 128),
        (0x0000_0000_0000_0000_0000_ffff_0000_0000, 96),
        (0x0064_ff9b_0001_0000_0000_0000_0000_0000, 48),
        (0x0100_0000_0000_0000_0000_0000_0000_0000, 64),
        (0x2001_0000_0000_0000_0000_0000_0000_0000, 23),
        (0x2001_0db8_0000_0000_0000_0000_0000_0000, 32),
        (0x2002_0000_0000_0000_0000_0000_0000_0000, 16),
        (0xfc00_0000_0000_0000_0000_0000_0000_0000, 7),
        (0xfe80_0000_0000_0000_0000_0000_0000_0000, 10),
    ]
    .iter()
    .any(|(network, prefix)| v6_in(ip, *network, *prefix))
}

fn v6_reserved(ip: Ipv6Addr) -> bool {
    [
        (0x0000, 8),
        (0x0100, 8),
        (0x0200, 7),
        (0x0400, 6),
        (0x0800, 5),
        (0x1000, 4),
        (0x4000, 3),
        (0x6000, 3),
        (0x8000, 3),
        (0xa000, 3),
        (0xc000, 3),
        (0xe000, 4),
        (0xf000, 5),
        (0xf800, 6),
        (0xfe00, 9),
    ]
    .iter()
    .any(|(top, prefix)| v6_in(ip, u128::from(*top as u16) << 112, *prefix))
}

/// Why `ip` is off-limits, or `None` for a public address, worded as Python's.
pub fn block_reason(ip: IpAddr) -> Option<String> {
    let address = unwrap(ip);
    let label = match address {
        IpAddr::V4(v4) => {
            if v4.is_unspecified() {
                "unspecified"
            } else if v4.is_loopback() {
                "loopback"
            } else if v4_in(v4, [169, 254, 0, 0], 16) {
                "link-local (cloud metadata range)"
            } else if v4_in(v4, [100, 64, 0, 0], 10) {
                // Beyond Python, whose `ipaddress` calls this range neither private
                // nor global: carrier-grade NAT, Tailscale peers, and Alibaba
                // Cloud's metadata service at 100.100.100.200.
                "shared (carrier-grade NAT and cloud metadata range)"
            } else if v4_in(v4, [192, 88, 99, 0], 24) {
                // Beyond Python: the deprecated 6to4 relay anycast range.
                "reserved"
            } else if v4_private(v4) {
                "private"
            } else if v4_in(v4, [224, 0, 0, 0], 4) {
                "multicast"
            } else if v4_in(v4, [240, 0, 0, 0], 4) {
                "reserved"
            } else {
                return None;
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_unspecified() {
                "unspecified"
            } else if v6.is_loopback() {
                "loopback"
            } else if v6_in(v6, 0xfe80 << 112, 10) {
                "link-local (cloud metadata range)"
            } else if v6_private(v6) {
                "private"
            } else if v6_in(v6, 0xff00 << 112, 8) {
                "multicast"
            } else if v6_reserved(v6) {
                "reserved"
            } else {
                return None;
            }
        }
    };
    let via = if address == ip {
        String::new()
    } else {
        format!(" via {}", python_ip(ip))
    };
    Some(format!("{label} address {}{via}", python_ip(address)))
}

/// An address as Python's `str()` writes it: IPv6 as compressed hex hextets,
/// never with an embedded dotted quad.
pub fn python_ip(ip: IpAddr) -> String {
    let IpAddr::V6(v6) = ip else {
        return ip.to_string();
    };
    let hextets = v6.segments();
    // The longest run of two or more zero hextets (the first, on a tie) becomes `::`.
    let (mut best_start, mut best_len) = (None, 0);
    let mut index = 0;
    while index < 8 {
        if hextets[index] == 0 {
            let start = index;
            while index < 8 && hextets[index] == 0 {
                index += 1;
            }
            if index - start > best_len && index - start > 1 {
                best_start = Some(start);
                best_len = index - start;
            }
        } else {
            index += 1;
        }
    }
    let render = |part: &[u16]| {
        part.iter()
            .map(|hextet| format!("{hextet:x}"))
            .collect::<Vec<_>>()
            .join(":")
    };
    match best_start {
        Some(start) => format!(
            "{}::{}",
            render(&hextets[..start]),
            render(&hextets[start + best_len..])
        ),
        None => render(&hextets),
    }
}

/// A URL the SSRF guard refuses to fetch. The message is model-facing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedUrl(pub String);

/// The scheme as Python's `urlsplit` finds it: the text before the first `:`,
/// when it is a valid scheme, lowercased.
fn url_scheme(url: &str) -> String {
    match url.split_once(':') {
        Some((scheme, _))
            if scheme
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) =>
        {
            scheme.to_lowercase()
        }
        _ => String::new(),
    }
}

/// Check `url` and return the addresses to connect to (empty when private
/// targets are allowed, leaving resolution to the HTTP client).
fn vet_url(url: &str, allow_private: bool) -> Result<(url::Url, Vec<SocketAddr>), BlockedUrl> {
    let scheme = url_scheme(url);
    if scheme != "http" && scheme != "https" {
        let shown = if scheme.is_empty() {
            "no scheme"
        } else {
            &scheme
        };
        return Err(BlockedUrl(format!(
            "Refusing to fetch '{url}': only http and https URLs are allowed (got '{shown}')."
        )));
    }
    let parsed = url::Url::parse(url)
        .ok()
        .filter(|parsed| parsed.host_str().is_some_and(|host| !host.is_empty()))
        .ok_or_else(|| BlockedUrl(format!("Refusing to fetch '{url}': no host in URL.")))?;
    if allow_private {
        return Ok((parsed, Vec::new()));
    }
    let host = parsed
        .host_str()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addresses = resolve(&host, port)?;
    for address in &addresses {
        if let Some(reason) = block_reason(address.ip()) {
            return Err(BlockedUrl(format!(
                "Refusing to fetch '{url}': host '{host}' resolves to a {reason}, which is \
                 inside the local trust boundary. {ALLOW_HINT}"
            )));
        }
    }
    Ok((parsed, addresses))
}

/// Every A and AAAA answer for `host`: a hostname is only as safe as its worst answer.
fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>, BlockedUrl> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let addresses: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|error| BlockedUrl(format!("Could not resolve host '{host}': {error}")))?
        .collect();
    if addresses.is_empty() {
        return Err(BlockedUrl(format!(
            "Could not resolve host '{host}' to any address."
        )));
    }
    Ok(addresses)
}

fn content_type_ok(content_type: &str) -> bool {
    let kind = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    kind.is_empty()
        || ALLOWED_CONTENT_TYPES
            .iter()
            .any(|prefix| kind.starts_with(prefix))
        || ALLOWED_CONTENT_TYPE_SUFFIXES
            .iter()
            .any(|suffix| kind.ends_with(suffix))
}

/// The response charset: the header's, or UTF-8, as httpx resolves it.
fn charset(content_type: &str) -> String {
    let lower = content_type.to_lowercase();
    match lower.split_once("charset=") {
        Some((_, rest)) => {
            let value = rest
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .trim_matches('"');
            if value.is_empty() {
                "utf-8".to_owned()
            } else {
                value.to_owned()
            }
        }
        None => "utf-8".to_owned(),
    }
}

/// Decode `bytes` as Python's `bytes.decode(charset, errors="replace")`.
fn decode(bytes: &[u8], charset: &str) -> Result<String, ToolError> {
    // WHATWG maps "latin-1" to windows-1252; Python's codec is true ISO-8859-1.
    if matches!(
        charset,
        "latin-1" | "latin1" | "iso-8859-1" | "iso8859-1" | "l1"
    ) {
        return Ok(bytes.iter().map(|&byte| byte as char).collect());
    }
    match encoding_rs::Encoding::for_label(charset.as_bytes()) {
        Some(encoding) => Ok(encoding.decode_without_bom_handling(bytes).0.into_owned()),
        None => Err(ToolError::Raised {
            exception: "LookupError".to_owned(),
            message: format!("unknown encoding: {charset}"),
        }),
    }
}

/// The next URL for a redirect, as Python's `urljoin`.
fn join(current: &str, location: &str) -> String {
    if url::Url::parse(location).is_ok() {
        return location.to_owned();
    }
    url::Url::parse(current)
        .and_then(|base| base.join(location))
        .map(|joined| joined.to_string())
        .unwrap_or_else(|_| location.to_owned())
}

struct Fetched {
    url: String,
    status: u16,
    content_type: String,
    text: String,
}

enum FetchError {
    Blocked(BlockedUrl),
    Http(String),
    Tool(ToolError),
}

/// Follow redirects manually, vetting every hop.
async fn fetch(
    url: &str,
    settings: &WebSettings,
    timeout: Duration,
) -> Result<Fetched, FetchError> {
    let mut current = url.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        let (parsed, addresses) =
            vet_url(&current, settings.allow_private).map_err(FetchError::Blocked)?;
        // Beyond Python: never through a proxy (reqwest reads HTTP(S)_PROXY by
        // default). A proxy resolves the host itself, so it would connect wherever
        // DNS says next, not to the addresses vetted above.
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .user_agent(&settings.user_agent);
        if let (Some(host), false) = (parsed.host_str(), addresses.is_empty()) {
            // Connect to the addresses that were vetted, not a fresh lookup.
            builder = builder.resolve_to_addrs(host, &addresses);
        }
        let client = builder
            .build()
            .map_err(|error| FetchError::Http(error_text(&error)))?;
        let mut response = client
            .get(parsed)
            .send()
            .await
            .map_err(|error| FetchError::Http(error_text(&error)))?;

        let status = response.status().as_u16();
        let header = |name: reqwest::header::HeaderName| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let location = header(reqwest::header::LOCATION);
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            if let Some(location) = location.filter(|location| !location.is_empty()) {
                let next = join(&current, &location);
                if next == current {
                    return Err(FetchError::Blocked(BlockedUrl(format!(
                        "Refusing to fetch '{url}': redirect loop at '{current}'."
                    ))));
                }
                current = next;
                continue;
            }
        }

        let content_type = header(reqwest::header::CONTENT_TYPE).unwrap_or_default();
        if !content_type_ok(&content_type) {
            return Err(FetchError::Blocked(BlockedUrl(format!(
                "Refusing to read '{current}': content type '{content_type}' is not text. \
                 web_fetch only returns text/HTML/JSON/XML content."
            ))));
        }
        let mut body = Vec::new();
        let mut truncated = false;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| FetchError::Http(error_text(&error)))?
        {
            body.extend_from_slice(&chunk);
            if body.len() >= MAX_BODY_BYTES {
                body.truncate(MAX_BODY_BYTES);
                truncated = true;
                break;
            }
        }
        let mut text = decode(&body, &charset(&content_type)).map_err(FetchError::Tool)?;
        if truncated {
            text.push_str(&format!(
                "\n\n... [body truncated at {MAX_BODY_BYTES} bytes]"
            ));
        }
        return Ok(Fetched {
            url: current,
            status,
            content_type,
            text,
        });
    }
    Err(FetchError::Blocked(BlockedUrl(format!(
        "Refusing to fetch '{url}': more than {MAX_REDIRECTS} redirects (last: '{current}')."
    ))))
}

fn error_text(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

// --------------------------------------------------------------------------
// HTML to text, as Python's regex reducer (used when BeautifulSoup is absent).
// --------------------------------------------------------------------------

static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
static DROP_OPEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<(script|style|nav|header|footer|aside|form|noscript|figure|table)\b[^>]*>")
        .unwrap()
});
static SUP_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<sup\b[^>]*>.*?</sup>").unwrap());
static MAIN_REGIONS: LazyLock<[Regex; 3]> = LazyLock::new(|| {
    [
        Regex::new(r"(?is)<main\b[^>]*>(.*?)</main>").unwrap(),
        Regex::new(r"(?is)<article\b[^>]*>(.*?)</article>").unwrap(),
        Regex::new(r#"(?is)<div\b[^>]*id=["']mw-content-text["'][^>]*>(.*)"#).unwrap(),
    ]
});
static BLANK_RUNS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

/// Python's `<(script|...)\b[^>]*>.*?</\1>` substitution. The `regex` crate has
/// no backreferences, so each opening tag is found and its own closing tag
/// searched for, scanning left to right as `re.sub` does.
fn drop_blocks(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut position = 0;
    let mut search_from = 0;
    // Beyond Python: tags already known to have no closing tag further on. Without
    // this, a page of unclosed `<form>`s scans to its end once per tag, which for
    // a 2 MB body is enough to freeze the turn; the output is the same, since no
    // close after one open means none after a later one.
    let mut unclosed: Vec<String> = Vec::new();
    while search_from < html.len() {
        let Some(open) = DROP_OPEN.captures_at(html, search_from) else {
            break;
        };
        let whole = open.get(0).expect("group 0 always matches");
        let closing = format!("</{}>", open[1].to_ascii_lowercase());
        if unclosed.contains(&closing) {
            search_from = next_char(html, whole.start());
            continue;
        }
        let found = find_ignore_ascii_case(&html[whole.end()..], &closing);
        if found.is_none() {
            unclosed.push(closing.clone());
        }
        match found {
            Some(offset) => {
                out.push_str(&html[position..whole.start()]);
                out.push(' ');
                position = whole.end() + offset + closing.len();
                search_from = position;
            }
            // No closing tag: the regex would fail here and retry one character on.
            None => search_from = next_char(html, whole.start()),
        }
    }
    out.push_str(&html[position..]);
    out
}

fn next_char(text: &str, index: usize) -> usize {
    index + text[index..].chars().next().map_or(1, char::len_utf8)
}

/// The byte offset of ASCII `needle` in `haystack`, ignoring ASCII case. A match
/// starts with `<`, so the offset is always a character boundary.
fn find_ignore_ascii_case(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Reduce HTML to readable text: keep the main region, drop page chrome and
/// citation markers, strip tags, and remove blank lines.
pub fn html_to_text(html: &str) -> String {
    let mut html = html.to_owned();
    for region in MAIN_REGIONS.iter() {
        if let Some(found) = region.captures(&html).and_then(|captures| captures.get(1)) {
            html = found.as_str().to_owned();
            break;
        }
    }
    let html = drop_blocks(&html);
    let html = SUP_REF.replace_all(&html, " ");
    let text = TAG
        .replace_all(&html, " ")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    let lines: Vec<&str> = crate::difflib::splitlines(&text)
        .into_iter()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    BLANK_RUNS
        .replace_all(&lines.join("\n"), "\n\n")
        .into_owned()
}

pub struct WebFetchTool;

impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a public URL over HTTP(S) and return its text content (HTML is reduced to text). \
         Read-only, but requires network access to be enabled. Private, loopback, and \
         link-local addresses are refused."
    }

    fn parameters(&self) -> Value {
        json!({
            "properties": {
                "url": {
                    "description": "Absolute http(s) URL to fetch.",
                    "title": "Url",
                    "type": "string",
                },
            },
            "required": ["url"],
            "type": "object",
        })
    }

    fn execute(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let mut arguments = Arguments::new(args);
        let url = arguments.string("url", None);
        arguments.finish()?;
        ctx.policy.require_network()?;

        // Tools run synchronously inside the harness's runtime, so the fetch gets
        // a runtime of its own on a scoped thread.
        let settings = &ctx.web;
        let timeout = ctx.policy.timeout;
        let fetched = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|error| FetchError::Http(error.to_string()))?;
                    runtime.block_on(fetch(&url, settings, timeout))
                })
                .join()
                .unwrap_or_else(|_| Err(FetchError::Http("the fetch thread panicked".to_owned())))
        });
        let fetched = match fetched {
            Ok(fetched) => fetched,
            Err(FetchError::Blocked(BlockedUrl(message))) => {
                return Ok(failure(self.name(), message))
            }
            Err(FetchError::Http(message)) => {
                return Ok(failure(self.name(), format!("Fetch failed: {message}")));
            }
            Err(FetchError::Tool(error)) => return Err(error),
        };

        let mut text = fetched.text;
        if fetched.content_type.to_lowercase().contains("html")
            || text
                .trim_start()
                .to_lowercase()
                .starts_with("<!doctype html")
        {
            text = html_to_text(&text);
        }
        let mut text = text.trim().to_owned();
        let cap = settings.max_chars;
        if cap > 0 && text.chars().count() > cap {
            let kept: String = text.chars().take(cap).collect();
            text = format!("{}\n\n... [truncated to {cap} chars]", kept.trim_end());
        }
        let ok = (200..300).contains(&fetched.status);
        let header = format!(
            "HTTP {} {} ({})\n\n",
            fetched.status, fetched.url, fetched.content_type
        );
        let mut result = success(self.name(), ctx.policy.truncate(format!("{header}{text}")));
        if !ok {
            result.ok = false;
            result.error = Some(format!("HTTP {} ({})", fetched.status, fetched.url));
        }
        Ok(result)
    }
}
