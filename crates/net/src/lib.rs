use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod cache;
mod cookies;
mod psl;

pub use cache::Cache;
pub use cookies::{Context, Cookie, Cookies, SameSite};

/// Bytes of response bodies read from the network (not from a cache or a file), as they
/// arrived (compressed or not).
static RECEIVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn bytes_received() -> u64 {
    RECEIVED.load(std::sync::atomic::Ordering::Relaxed)
}
pub use psl::site;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

const USER_AGENT: &str = "Broxee/0.1";
const MAX_REDIRECTS: usize = 10;

#[derive(Debug, Clone, PartialEq)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    /// Path plus query string, always starting with `/`.
    pub path: String,
}

impl Url {
    /// Parse an absolute URL the way the URL standard does for what Broxee supports: leading
    /// and trailing spaces and controls are dropped, tabs and newlines inside are removed,
    /// spaces and other unsafe bytes in the path and query are percent-encoded, and a host
    /// with a space or another forbidden character is an error.
    pub fn parse(input: &str) -> Result<Url, String> {
        let cleaned = clean(input);
        let input = cleaned.as_str();
        let (scheme, rest) = input
            .split_once("://")
            .ok_or_else(|| format!("missing scheme: {input}"))?;
        let scheme = scheme.to_ascii_lowercase();

        if scheme == "file" {
            let path = rest.split('#').next().unwrap_or_default();
            return Ok(Url {
                scheme,
                host: String::new(),
                port: 0,
                path: encode_path(path),
            });
        }

        let default_port =
            default_port(&scheme).ok_or_else(|| format!("unsupported scheme: {scheme}"))?;
        let (authority, path) = match rest.find(['/', '?', '#']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let path = path.split('#').next().unwrap_or("/");
        let path = if path.starts_with('?') {
            format!("/{path}")
        } else {
            path.to_string()
        };

        // Credentials (`user:pass@`) are never sent: Broxee has no HTTP authentication.
        let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, "")) => (h, default_port),
            Some((h, p)) => (h, p.parse().map_err(|_| format!("bad port: {p}"))?),
            None => (authority, default_port),
        };
        if host.is_empty() {
            return Err(format!("empty host: {input}"));
        }
        if let Some(c) = host.chars().find(|&c| forbidden_in_host(c)) {
            let what = if c == ' ' {
                "a space".to_string()
            } else {
                format!("\"{c}\"")
            };
            return Err(format!("a host name can't contain {what}: {input}"));
        }
        Ok(Url {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
            path: encode_path(&normalize_path(&path)),
        })
    }

    /// The path for display or the file system: percent escapes decoded.
    pub fn decoded_path(&self) -> String {
        String::from_utf8_lossy(&percent_decode(&self.path)).into_owned()
    }

    /// Resolve `href` (absolute, protocol-relative, root-relative or relative) against this URL.
    pub fn join(&self, href: &str) -> Result<Url, String> {
        let cleaned = clean(href);
        let href = cleaned.as_str();
        if href.contains("://") {
            return Url::parse(href);
        }
        if let Some(rest) = href.strip_prefix("//") {
            return Url::parse(&format!("{}://{rest}", self.scheme));
        }

        let href = href.split('#').next().unwrap_or("");
        let base = self.path.split('?').next().unwrap_or("/");
        let mut url = self.clone();
        url.path = if href.is_empty() {
            return Ok(url);
        } else if href.starts_with('/') {
            href.to_string()
        } else if href.starts_with('?') {
            format!("{base}{href}")
        } else {
            let dir = &base[..base.rfind('/').map(|i| i + 1).unwrap_or(0)];
            format!("{dir}{href}")
        };
        url.path = encode_path(&normalize_path(&url.path));
        Ok(url)
    }
}

/// Drop leading and trailing C0 controls and spaces, and every tab and newline (URL standard,
/// "basic URL parser", first steps).
fn clean(input: &str) -> String {
    input
        .trim_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect()
}

/// Characters the URL standard forbids in a host name.
fn forbidden_in_host(c: char) -> bool {
    c <= ' '
        || matches!(
            c,
            '#' | '%' | '/' | ':' | '<' | '>' | '?' | '@' | '[' | '\\' | ']' | '^' | '|' | '\u{7f}'
        )
}

/// Percent-encode what may not appear raw in a path or query: controls, space, `"`, `<`,
/// `>`, `` ` ``, `{`, `}`, DEL and every non-ASCII byte (as UTF-8). Existing escapes stay.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        if b <= b' ' || b >= 0x7f || matches!(b, b'"' | b'<' | b'>' | b'`' | b'{' | b'}') {
            out.push_str(&format!("%{b:02X}"));
        } else {
            out.push(b as char);
        }
    }
    out
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.scheme == "file" {
            return write!(f, "file://{}", self.path);
        }
        if Some(self.port) == default_port(&self.scheme) {
            write!(f, "{}://{}{}", self.scheme, self.host, self.path)
        } else {
            write!(
                f,
                "{}://{}:{}{}",
                self.scheme, self.host, self.port, self.path
            )
        }
    }
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

/// Collapse `.`/`..` segments and duplicate slashes, keeping the query string intact.
fn normalize_path(path: &str) -> String {
    let (p, query) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path, None),
    };
    let trailing = p.ends_with('/') || p.ends_with("/.") || p.ends_with("/..");
    let mut segments: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    let mut out = format!("/{}", segments.join("/"));
    if trailing && out != "/" {
        out.push('/');
    }
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    out
}

/// The bytes of a `data:` URL (RFC 2397): base64 or percent-encoded. `None` if malformed.
pub fn decode_data_url(url: &str) -> Option<Vec<u8>> {
    let rest = url.trim().strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    if meta.ends_with(";base64") {
        decode_base64(payload)
    } else {
        Some(percent_decode(payload))
    }
}

fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            // Percent-encoded padding or whitespace inside an attribute: skip, as browsers do.
            b'%' => continue,
            _ => return None,
        };
        acc = acc << 6 | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

#[derive(Debug)]
pub struct Response {
    /// Final URL after redirects.
    pub url: Url,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// The body, decoded.
    pub body: Vec<u8>,
    /// The body as it arrived, when it was compressed: the smallest form to keep (a sleeping
    /// tab keeps this instead of the page). `Encoding::Identity` and empty otherwise.
    pub wire: Wire,
}

/// A body in its `Content-Encoding`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Wire {
    pub encoding: Encoding,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Encoding {
    #[default]
    Identity,
    Brotli,
    Gzip,
    Deflate,
}

impl Wire {
    /// Decode, refusing anything past `limit` bytes.
    pub fn decode(&self, limit: usize) -> Result<Vec<u8>, String> {
        decode(self.encoding, &self.bytes, limit)
    }
}

fn decode(encoding: Encoding, bytes: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    match encoding {
        Encoding::Identity => Ok(bytes.to_vec()),
        // Brotli first in Accept-Encoding: about a quarter smaller than gzip for HTML and up to
        // half for CSS, and it decodes faster here too.
        Encoding::Brotli => flate::brotli(bytes, limit).map_err(|e| e.to_string()),
        Encoding::Gzip => flate::gunzip(bytes, limit).map_err(|e| e.to_string()),
        // "deflate" is meant to be zlib-wrapped, but some servers send raw DEFLATE.
        Encoding::Deflate => flate::zlib_decompress(bytes, limit)
            .or_else(|_| flate::inflate(bytes, limit))
            .map_err(|e| e.to_string()),
    }
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Why a single request failed, and whether repeating it is safe and likely to help.
enum FetchError {
    /// The connection was reset, aborted or found closed before a response began. A GET is
    /// idempotent, so browsers retry it on a fresh connection.
    Retryable(String),
    Fatal(String),
}

impl From<String> for FetchError {
    fn from(message: String) -> Self {
        FetchError::Fatal(message)
    }
}

impl From<FetchError> for String {
    fn from(e: FetchError) -> Self {
        match e {
            FetchError::Retryable(m) | FetchError::Fatal(m) => m,
        }
    }
}

fn io_error(e: std::io::Error) -> FetchError {
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset, UnexpectedEof};
    match e.kind() {
        ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof => {
            FetchError::Retryable(e.to_string())
        }
        _ => FetchError::Fatal(e.to_string()),
    }
}

/// Bodies larger than this are refused, compressed or not: a page cannot make the browser
/// allocate past its memory budget.
pub const MAX_BODY: usize = 64 << 20;
const MAX_HEAD: usize = 64 << 10;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(20);
/// Idle connections kept for reuse, and how long one may sit idle.
const MAX_IDLE: usize = 8;
const IDLE_FOR: Duration = Duration::from_secs(30);

/// GET a URL without cookies, following redirects.
pub fn fetch(url: &Url) -> Result<Response, String> {
    fetch_in(url, None)
}

/// GET a URL, following redirects, sending and storing cookies of a profile's jar when given.
/// A request that fails on a reused or reset connection is retried once on a fresh one.
pub fn fetch_in(url: &Url, cookies: Option<(&Cookies, &Context)>) -> Result<Response, String> {
    send_in(url, None, cookies)
}

/// A request body: what a form sends with POST.
#[derive(Clone, Debug, PartialEq)]
pub struct Body {
    /// Without CR or LF (`application/x-www-form-urlencoded` for forms).
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// GET `url`, or POST `body` to it, following redirects as `fetch_in` does. After a 301,
/// 302 or 303 the body is dropped and the next request is a GET, as browsers do; 307 and 308
/// send it again. A POST is not repeated on a fresh connection unless the first one was a
/// reused connection the server had already closed.
pub fn send_in(
    url: &Url,
    body: Option<&Body>,
    cookies: Option<(&Cookies, &Context)>,
) -> Result<Response, String> {
    send_with(url, body, None, cookies)
}

/// What a script's request adds to a plain GET or POST: its method and its own headers.
#[derive(Clone, Debug, Default)]
pub struct Extra {
    /// Uppercase, a token (`PUT`, `DELETE`…).
    pub method: String,
    /// Already checked by the caller: no forbidden names, no CR or LF.
    pub headers: Vec<(String, String)>,
}

/// As `send_in`, with a script's method and headers. A 301, 302 or 303 turns the request
/// into a plain GET.
pub fn send_with(
    url: &Url,
    body: Option<&Body>,
    extra: Option<&Extra>,
    cookies: Option<(&Cookies, &Context)>,
) -> Result<Response, String> {
    let mut url = url.clone();
    let mut body = body;
    let mut extra = extra;
    let cache = cookies.and_then(|(jar, _)| jar.cache());
    for _ in 0..MAX_REDIRECTS {
        // A plain GET (no body, no credentials of its own) may come from the cache.
        let cacheable = body.is_none()
            && matches!(url.scheme.as_str(), "http" | "https")
            && extra.is_none_or(|e| {
                e.method == "GET"
                    && !e
                        .headers
                        .iter()
                        .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            });
        let stored = cache.filter(|_| cacheable).and_then(|c| c.get(&url));
        if stored.as_ref().is_some_and(|e| e.fresh(cache::now())) {
            return stored.expect("checked above").into_response(&url);
        }
        // A stale entry asks whether it still holds.
        let conditional;
        let ask = match &stored {
            Some(entry) if entry.etag.is_some() || entry.last_modified.is_some() => {
                let mut headers = extra.map(|e| e.headers.clone()).unwrap_or_default();
                if let Some(tag) = &entry.etag {
                    headers.push(("If-None-Match".into(), tag.replace(['\r', '\n'], "")));
                }
                if let Some(date) = &entry.last_modified {
                    headers.push(("If-Modified-Since".into(), date.replace(['\r', '\n'], "")));
                }
                conditional = Extra {
                    method: "GET".into(),
                    headers,
                };
                Some(&conditional)
            }
            _ => extra,
        };
        let header = cookies.and_then(|(jar, context)| jar.header(&url, context));
        let resp = match fetch_once(&url, header.as_deref(), body, ask) {
            Err(FetchError::Retryable(_)) if body.is_none() => {
                fetch_once(&url, header.as_deref(), None, ask)?
            }
            other => other?,
        };
        if let (Some(c), Some(entry)) = (cache, stored) {
            if resp.status == 304 {
                return c.refresh(&url, entry, &resp.headers).into_response(&url);
            }
            c.remove(&url);
        }
        if let (Some(c), true) = (cache, cacheable) {
            c.put(&url, &resp);
        }
        if let Some((jar, context)) = cookies {
            for (name, value) in &resp.headers {
                if name.eq_ignore_ascii_case("set-cookie") {
                    jar.store(&url, value, context);
                }
            }
        }
        if (300..400).contains(&resp.status) {
            if let Some(location) = resp.header("location") {
                url = url.join(location)?;
                if !matches!(resp.status, 307 | 308) {
                    body = None;
                    extra = None;
                }
                continue;
            }
        }
        return Ok(resp);
    }
    Err(format!("too many redirects ending at {url}"))
}

fn fetch_once(
    url: &Url,
    cookie: Option<&str>,
    body: Option<&Body>,
    extra: Option<&Extra>,
) -> Result<Response, FetchError> {
    if url.scheme == "file" {
        let path = url.decoded_path();
        let body = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
        return Ok(Response {
            url: url.clone(),
            status: 200,
            headers: Vec::new(),
            body,
            wire: Wire::default(),
        });
    }
    let key = (url.scheme.clone(), url.host.clone(), url.port);
    let (mut conn, reused) = match take_idle(&key) {
        Some(conn) => (conn, true),
        None => (Conn::open(url)?, false),
    };
    let counted = |resp: Response| {
        let n = if resp.wire.bytes.is_empty() {
            resp.body.len()
        } else {
            resp.wire.bytes.len()
        };
        RECEIVED.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        resp
    };
    match request(&mut conn, url, cookie, body, extra) {
        Ok((resp, reusable)) => {
            if reusable {
                put_idle(key, conn);
            }
            Ok(counted(resp))
        }
        // A pooled connection the server already closed: not the server's answer.
        Err(FetchError::Retryable(_)) if reused => {
            let mut conn = Conn::open(url)?;
            let (resp, reusable) = request(&mut conn, url, cookie, body, extra)?;
            if reusable {
                put_idle(key, conn);
            }
            Ok(counted(resp))
        }
        Err(e) => Err(e),
    }
}

fn request(
    conn: &mut Conn,
    url: &Url,
    cookie: Option<&str>,
    body: Option<&Body>,
    extra: Option<&Extra>,
) -> Result<(Response, bool), FetchError> {
    let host_header = if Some(url.port) == default_port(&url.scheme) {
        url.host.clone()
    } else {
        format!("{}:{}", url.host, url.port)
    };
    // Cookie values never hold CR or LF: the jar refuses them when storing.
    let cookie = cookie.map_or(String::new(), |c| format!("Cookie: {c}\r\n"));
    let (method, content) = match body {
        Some(b) => (
            "POST",
            format!(
                "Content-Type: {}\r\nContent-Length: {}\r\n",
                b.content_type.replace(['\r', '\n'], ""),
                b.bytes.len()
            ),
        ),
        None => ("GET", String::new()),
    };
    let method = extra.map_or(method, |e| e.method.as_str());
    let mut own = String::new();
    let mut accept = "text/html,*/*;q=0.8";
    for (name, value) in extra.map_or(&[][..], |e| &e.headers[..]) {
        if name.eq_ignore_ascii_case("accept") {
            accept = value;
        } else {
            own.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    let content = match (body, extra) {
        (None, Some(e)) if !matches!(e.method.as_str(), "GET" | "HEAD") => {
            "Content-Length: 0\r\n".to_string()
        }
        _ => content,
    };
    let mut request = format!(
        "{method} {} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: {USER_AGENT}\r\n\
         Accept: {accept}\r\nAccept-Encoding: br, gzip, deflate\r\n\
         {cookie}{own}{content}Connection: keep-alive\r\n\r\n",
        url.path
    )
    .into_bytes();
    if let Some(b) = body {
        request.extend_from_slice(&b.bytes);
    }
    conn.stream.write_all(&request).map_err(io_error)?;
    conn.stream.flush().map_err(io_error)?;
    let raw = read_response(&mut conn.reader())?;
    let reusable = raw.reusable;
    Ok((finish(url, raw)?, reusable))
}

/// A response as framed on the wire, before content decoding.
struct RawResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// The connection is positioned at the next response and may be reused.
    reusable: bool,
}

/// Buffered reading over a connection; bytes past one response stay for the next.
struct Reader<'a, S: Read> {
    stream: &'a mut S,
    buf: &'a mut Vec<u8>,
}

impl<S: Read> Reader<'_, S> {
    /// Read more bytes into the buffer. Returns false at end of stream.
    fn fill(&mut self) -> Result<bool, FetchError> {
        let start = self.buf.len();
        self.buf.resize(start + 16 * 1024, 0);
        let n = match self.stream.read(&mut self.buf[start..]) {
            Ok(n) => n,
            // Servers often close TLS without close_notify; treat it as the end of the stream.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
            Err(e) => {
                self.buf.truncate(start);
                return Err(io_error(e));
            }
        };
        self.buf.truncate(start + n);
        Ok(n > 0)
    }

    /// Take a line ending in CRLF (without it).
    fn line(&mut self) -> Result<Vec<u8>, FetchError> {
        loop {
            if let Some(i) = find(self.buf, b"\r\n") {
                let line = self.buf[..i].to_vec();
                self.buf.drain(..i + 2);
                return Ok(line);
            }
            if self.buf.len() > MAX_HEAD {
                return Err("line too long".to_string().into());
            }
            if !self.fill()? {
                return Err(FetchError::Fatal("connection closed mid-response".into()));
            }
        }
    }

    /// Move up to `n` buffered or incoming bytes into `out`.
    fn copy(&mut self, n: usize, out: &mut Vec<u8>) -> Result<(), FetchError> {
        if out.len() + n > MAX_BODY {
            return Err("response body too large".to_string().into());
        }
        let mut left = n;
        while left > 0 {
            if self.buf.is_empty() && !self.fill()? {
                return Err(FetchError::Fatal("connection closed mid-body".into()));
            }
            let take = left.min(self.buf.len());
            out.extend_from_slice(&self.buf[..take]);
            self.buf.drain(..take);
            left -= take;
        }
        Ok(())
    }
}

/// Read one HTTP/1.1 response: status, headers, and a body framed by `Content-Length`,
/// chunked encoding, or the end of the connection.
fn read_response<S: Read>(r: &mut Reader<S>) -> Result<RawResponse, FetchError> {
    let head_end = loop {
        if let Some(i) = find(r.buf, b"\r\n\r\n") {
            break i;
        }
        if r.buf.len() > MAX_HEAD {
            return Err("response headers too large".to_string().into());
        }
        let started = !r.buf.is_empty();
        if !r.fill()? {
            return Err(if started {
                FetchError::Fatal("malformed response: no header terminator".into())
            } else {
                FetchError::Retryable("connection closed before a response".into())
            });
        }
    };
    let head = String::from_utf8_lossy(&r.buf[..head_end]).into_owned();
    r.buf.drain(..head_end + 4);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("bad status line: {status_line}"))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let closes = header("connection").is_some_and(|v| v.eq_ignore_ascii_case("close"))
        || status_line.starts_with("HTTP/1.0");
    let chunked =
        header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let length = header("content-length").and_then(|v| v.parse::<usize>().ok());

    let mut body = Vec::new();
    let framed = if status == 204 || status == 304 || (100..200).contains(&status) {
        true
    } else if chunked {
        loop {
            let line = r.line()?;
            let line = String::from_utf8_lossy(&line);
            let size_hex = line.split(';').next().unwrap_or_default().trim();
            let size = usize::from_str_radix(size_hex, 16)
                .map_err(|_| format!("bad chunk size: {line}"))?;
            if size == 0 {
                // Trailer fields, then an empty line.
                while !r.line()?.is_empty() {}
                break;
            }
            r.copy(size, &mut body)?;
            if !r.line()?.is_empty() {
                return Err("bad chunk terminator".to_string().into());
            }
        }
        true
    } else if let Some(len) = length {
        if len > MAX_BODY {
            return Err("response body too large".to_string().into());
        }
        body.reserve_exact(len);
        r.copy(len, &mut body)?;
        true
    } else {
        // Delimited by the end of the connection.
        body.append(r.buf);
        while r.fill()? {
            if r.buf.len() + body.len() > MAX_BODY {
                return Err("response body too large".to_string().into());
            }
            body.append(r.buf);
        }
        false
    };
    Ok(RawResponse {
        status,
        headers,
        body,
        reusable: framed && !closes,
    })
}

/// Undo `Content-Encoding` and build the response.
fn finish(url: &Url, raw: RawResponse) -> Result<Response, String> {
    let mut resp = Response {
        url: url.clone(),
        status: raw.status,
        headers: raw.headers,
        body: Vec::new(),
        wire: Wire::default(),
    };
    let encoding = match resp
        .header("content-encoding")
        .map(|e| e.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("br") => Encoding::Brotli,
        Some("gzip" | "x-gzip") => Encoding::Gzip,
        Some("deflate") => Encoding::Deflate,
        _ => Encoding::Identity,
    };
    if encoding == Encoding::Identity {
        resp.body = raw.body;
    } else {
        resp.body = decode(encoding, &raw.body, MAX_BODY)?;
        resp.wire = Wire {
            encoding,
            bytes: raw.body,
        };
    }
    Ok(resp)
}

enum Stream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// An open connection and the bytes read past the last response.
struct Conn {
    stream: Stream,
    buf: Vec<u8>,
}

impl Conn {
    fn open(url: &Url) -> Result<Conn, FetchError> {
        let addrs = (url.host.as_str(), url.port)
            .to_socket_addrs()
            .map_err(|e| format!("resolve {}: {e}", url.host))?;
        let mut last = format!("no address for {}", url.host);
        let tcp = addrs
            .into_iter()
            .find_map(
                |addr| match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                    Ok(tcp) => Some(tcp),
                    Err(e) => {
                        last = format!("connect {}:{}: {e}", url.host, url.port);
                        None
                    }
                },
            )
            .ok_or(last)?;
        tcp.set_read_timeout(Some(IO_TIMEOUT)).ok();
        tcp.set_write_timeout(Some(IO_TIMEOUT)).ok();
        tcp.set_nodelay(true).ok();
        let stream = if url.scheme == "https" {
            let name =
                ServerName::try_from(url.host.clone()).map_err(|e| format!("{}: {e}", url.host))?;
            let conn = ClientConnection::new(tls_config(), name).map_err(|e| e.to_string())?;
            Stream::Tls(Box::new(StreamOwned::new(conn, tcp)))
        } else {
            Stream::Plain(tcp)
        };
        Ok(Conn {
            stream,
            buf: Vec::new(),
        })
    }

    fn reader(&mut self) -> Reader<'_, Stream> {
        Reader {
            stream: &mut self.stream,
            buf: &mut self.buf,
        }
    }
}

type Key = (String, String, u16);

/// Idle keep-alive connections, shared by every fetching thread.
static IDLE: Mutex<Vec<(Key, Conn, Instant)>> = Mutex::new(Vec::new());

fn take_idle(key: &Key) -> Option<Conn> {
    let mut idle = IDLE.lock().ok()?;
    idle.retain(|(_, _, since)| since.elapsed() < IDLE_FOR);
    let i = idle.iter().rposition(|(k, _, _)| k == key)?;
    Some(idle.swap_remove(i).1)
}

fn put_idle(key: Key, conn: Conn) {
    if !conn.buf.is_empty() {
        return; // unread bytes: the connection is out of step, do not reuse it
    }
    if let Ok(mut idle) = IDLE.lock() {
        if idle.len() >= MAX_IDLE {
            idle.remove(0);
        }
        idle.push((key, conn, Instant::now()));
    }
}

/// The TLS configuration, and with it the session-resumption cache.
static CONFIG: Mutex<Option<Arc<ClientConfig>>> = Mutex::new(None);

fn tls_config() -> Arc<ClientConfig> {
    let mut config = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    config
        .get_or_insert_with(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("ring supports the default TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

/// Close idle connections and forget TLS sessions, so nothing a server could use to link two
/// requests carries over (switching profile). Requests already in flight finish normally.
pub fn forget_sessions() {
    if let Ok(mut idle) = IDLE.lock() {
        idle.clear();
    }
    let mut config = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(current) = config.as_mut() {
        let mut fresh = ClientConfig::clone(current);
        // A new in-memory store: tickets from before are unreachable and dropped with it.
        fresh.resumption = rustls::client::Resumption::default();
        *current = Arc::new(fresh);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server answering `answer(path, request head)` with (status, headers, body), counting
    /// the requests it gets.
    fn cache_server(
        answer: impl Fn(&str, &str) -> (u16, String, String) + Send + 'static,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                counter.fetch_add(1, Ordering::SeqCst);
                let path = head.split(' ').nth(1).unwrap_or("").to_string();
                let (status, headers, body) = answer(&path, &head);
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        (format!("http://127.0.0.1:{port}"), hits)
    }

    #[test]
    fn caches_per_profile_with_freshness_revalidation_and_no_store() {
        use std::sync::atomic::Ordering;
        let (base, hits) = cache_server(|path, head| match path {
            "/fresh" => (
                200,
                "Cache-Control: max-age=600\r\n".into(),
                "fresh body".into(),
            ),
            "/etag" if head.contains("If-None-Match: \"v1\"") => (
                304,
                "ETag: \"v1\"\r\nCache-Control: no-cache\r\n".into(),
                String::new(),
            ),
            "/etag" => (
                200,
                "ETag: \"v1\"\r\nCache-Control: no-cache\r\n".into(),
                "tagged body".into(),
            ),
            "/secret" => (
                200,
                "Cache-Control: no-store, max-age=600\r\n".into(),
                "secret".into(),
            ),
            "/cookie" => (
                200,
                "Cache-Control: max-age=600\r\nSet-Cookie: s=1\r\n".into(),
                "cookie".into(),
            ),
            _ => (404, String::new(), String::new()),
        });
        let root = std::env::temp_dir().join(format!("broxee-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let work = Cookies::new();
        work.attach_cache(Cache::open(root.join("work"), 1 << 20));
        let personal = Cookies::new();
        personal.attach_cache(Cache::open(root.join("personal"), 1 << 20));
        let page = Url::parse(&format!("{base}/")).unwrap();
        let ctx = Context::subresource(&page);
        let get = |jar: &Cookies, path: &str| {
            let url = Url::parse(&format!("{base}{path}")).unwrap();
            let r = fetch_in(&url, Some((jar, &ctx))).unwrap();
            (r.status, String::from_utf8(r.body).unwrap())
        };
        let count = || hits.load(Ordering::SeqCst);

        assert_eq!(get(&work, "/fresh"), (200, "fresh body".into()));
        assert_eq!(count(), 1);
        assert_eq!(get(&work, "/fresh"), (200, "fresh body".into()));
        assert_eq!(count(), 1, "a fresh entry makes no request");
        assert_eq!(get(&personal, "/fresh"), (200, "fresh body".into()));
        assert_eq!(count(), 2, "the other profile has its own cache");

        assert_eq!(get(&work, "/etag"), (200, "tagged body".into()));
        assert_eq!(
            get(&work, "/etag"),
            (200, "tagged body".into()),
            "304 gives the stored body"
        );
        assert_eq!(count(), 4, "no-cache revalidates every time");

        for path in ["/secret", "/cookie"] {
            get(&work, path);
            get(&work, path);
        }
        assert_eq!(count(), 8, "no-store and Set-Cookie are never reused");
        let files = |dir: &str| {
            std::fs::read_dir(root.join(dir))
                .map(|d| d.flatten().count())
                .unwrap_or(0)
        };
        assert_eq!(files("work"), 2, "only /fresh and /etag were written");
        assert_eq!(files("personal"), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let entry = std::fs::read_dir(root.join("work"))
                .unwrap()
                .flatten()
                .next()
                .unwrap();
            assert_eq!(
                entry.metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // Without a profile (the command line), nothing is cached.
        let url = Url::parse(&format!("{base}/fresh")).unwrap();
        fetch(&url).unwrap();
        assert_eq!(count(), 9);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn evicts_the_least_recently_used_past_its_limit() {
        let (base, _) = cache_server(|path, _| {
            (
                200,
                "Cache-Control: max-age=600\r\n".into(),
                format!("{path}{}", "x".repeat(400)),
            )
        });
        let root = std::env::temp_dir().join(format!("broxee-cache-lru-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let jar = Cookies::new();
        jar.attach_cache(Cache::open(root.clone(), 1500));
        let page = Url::parse(&format!("{base}/")).unwrap();
        let ctx = Context::subresource(&page);
        for path in ["/a", "/b", "/c", "/d"] {
            let url = Url::parse(&format!("{base}{path}")).unwrap();
            fetch_in(&url, Some((&jar, &ctx))).unwrap();
        }
        let size = jar.cache().unwrap().size();
        assert!(size <= 1500, "{size} bytes stored");
        assert!(size > 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parses_urls() {
        let u = Url::parse("https://Example.com/a/b?x=1#frag").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("example.com", 443, "/a/b?x=1")
        );
        assert_eq!(Url::parse("http://localhost:8080").unwrap().path, "/");
        assert_eq!(Url::parse("http://h:8080/").unwrap().port, 8080);
        assert!(Url::parse("ftp://x").is_err());
    }

    #[test]
    fn handles_whitespace_as_the_url_standard_does() {
        let u = |s: &str| Url::parse(s).map(|u| u.to_string());
        assert_eq!(u("  https://e.com/a\n \t").unwrap(), "https://e.com/a");
        assert_eq!(u("https://e.c\tom/a\nb").unwrap(), "https://e.com/ab");
        assert_eq!(
            u("https://e.com/a b?q=c d#x y").unwrap(),
            "https://e.com/a%20b?q=c%20d"
        );
        assert_eq!(
            u("https://e.com/caf\u{e9}").unwrap(),
            "https://e.com/caf%C3%A9"
        );
        assert_eq!(
            u("https://e.com/a%20b").unwrap(),
            "https://e.com/a%20b",
            "escapes stay"
        );
        assert_eq!(
            u("https://e.com:/x").unwrap(),
            "https://e.com/x",
            "empty port"
        );
        assert_eq!(u("https://user:pw@e.com/").unwrap(), "https://e.com/");
        // Valid by the standard: the space is in the path. Callers that take typed text
        // reject several addresses in one (`shell::engine::to_url`, the address field).
        assert_eq!(
            u("https://e.org/wiki/Cat https://d.org/").unwrap(),
            "https://e.org/wiki/Cat%20https:/d.org/"
        );
        let err = u("https://e.com https://d.org/").unwrap_err();
        assert!(err.contains("can't contain a space"), "{err}");
        assert!(u("https://e c.com/").is_err());
        assert!(u("https://e<c.com/").is_err());

        let base = Url::parse("https://e.com/dir/page").unwrap();
        assert_eq!(
            base.join(" my page.html\n").unwrap().path,
            "/dir/my%20page.html"
        );
        assert!(!base.join("a b").unwrap().to_string().contains(' '));

        let file = Url::parse("file:///tmp/My File.html").unwrap();
        assert_eq!(file.to_string(), "file:///tmp/My%20File.html");
        assert_eq!(file.decoded_path(), "/tmp/My File.html");
        assert_eq!(percent_decode("a%2"), b"a%2", "a cut escape stays as it is");
        assert_eq!(percent_decode("%41%zz"), b"A%zz");
    }

    /// A local server that closes the first `drops` connections right after reading the
    /// request, then answers; returns its URL.
    fn dropping_server(drops: usize) -> Url {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (i, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { break };
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request);
                if i < drops {
                    continue; // dropped: closed before any response
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            }
        });
        Url::parse(&format!("http://{address}/")).unwrap()
    }

    #[test]
    fn posts_a_body_and_follows_a_see_other_with_get() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for stream in listener.incoming().take(2) {
                let Ok(mut stream) = stream else { break };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read the head, then as many body bytes as Content-Length says.
                loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len || n == 0 {
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                let reply: &[u8] = if text.starts_with("POST") {
                    b"HTTP/1.1 303 See Other\r\nLocation: /done\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndone"
                };
                let _ = stream.write_all(reply);
                seen.push(text);
            }
            seen
        });
        let url = Url::parse(&format!("http://{address}/login")).unwrap();
        let body = Body {
            content_type: "application/x-www-form-urlencoded".into(),
            bytes: b"user=a&pass=b+c".to_vec(),
        };
        let resp = send_in(&url, Some(&body), None).expect("posted");
        assert_eq!((resp.status, resp.body.as_slice()), (200, &b"done"[..]));
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /login HTTP/1.1"), "{}", seen[0]);
        assert!(seen[0].contains("Content-Type: application/x-www-form-urlencoded\r\n"));
        assert!(seen[0].contains("Content-Length: 15\r\n"));
        assert!(seen[0].ends_with("\r\n\r\nuser=a&pass=b+c"));
        assert!(
            seen[1].starts_with("GET /done HTTP/1.1"),
            "303 turns into a GET"
        );
    }

    #[test]
    fn retries_a_get_once_when_the_connection_closes_before_answering() {
        let resp = fetch(&dropping_server(1)).expect("the retry gets the answer");
        assert_eq!((resp.status, resp.body.as_slice()), (200, &b"ok"[..]));
        let err = fetch(&dropping_server(2)).expect_err("only one retry");
        assert!(err.contains("closed before a response"), "{err}");
    }

    #[test]
    fn joins_relative_urls() {
        let base = Url::parse("https://e.com/docs/guide/intro.html?q=1").unwrap();
        assert_eq!(
            base.join("next.html").unwrap().path,
            "/docs/guide/next.html"
        );
        assert_eq!(base.join("../api/").unwrap().path, "/docs/api/");
        assert_eq!(base.join("/root").unwrap().path, "/root");
        assert_eq!(
            base.join("?page=2").unwrap().path,
            "/docs/guide/intro.html?page=2"
        );
        assert_eq!(base.join("//cdn.e.com/x.css").unwrap().host, "cdn.e.com");
        assert_eq!(
            base.join("http://o.com/").unwrap().to_string(),
            "http://o.com/"
        );
    }

    /// Status, body, and whether the connection may be reused.
    type Outcome = Result<(u16, Vec<u8>, bool), String>;

    /// Read every response from `wire` in turn, as one kept-alive connection would.
    fn responses(wire: &[u8]) -> Vec<Outcome> {
        let mut stream = std::io::Cursor::new(wire.to_vec());
        let mut buf = Vec::new();
        let mut out = Vec::new();
        loop {
            let mut r = Reader {
                stream: &mut stream,
                buf: &mut buf,
            };
            match read_response(&mut r) {
                Ok(raw) => {
                    let url = Url::parse("http://e.com/").unwrap();
                    let reusable = raw.reusable;
                    out.push(finish(&url, raw).map(|resp| (resp.status, resp.body, reusable)));
                    if !reusable {
                        break;
                    }
                }
                Err(FetchError::Retryable(_)) => break, // clean end of the connection
                Err(e) => {
                    out.push(Err(String::from(e)));
                    break;
                }
            }
        }
        out
    }

    #[test]
    fn decodes_data_urls() {
        assert_eq!(
            decode_data_url("data:image/png;base64,iVBORw0KGgo=").unwrap(),
            b"\x89PNG\r\n\x1a\n"
        );
        assert_eq!(
            decode_data_url("data:text/plain,a%20b%2").unwrap(),
            b"a b%2"
        );
        assert_eq!(
            decode_data_url("data:;base64,SGk").unwrap(),
            b"Hi",
            "missing padding"
        );
        assert_eq!(decode_data_url("data:;base64,S*k"), None);
        assert_eq!(decode_data_url("https://e.com/"), None);
    }

    #[test]
    fn frames_responses_on_a_kept_alive_connection() {
        let wire = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-A: b\r\n\r\nhello\
                     HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\n\r\n\
                     HTTP/1.1 304 Not Modified\r\n\r\n\
                     HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nuntil the end";
        let got = responses(wire);
        assert_eq!(got.len(), 4);
        assert_eq!(got[0], Ok((200, b"hello".to_vec(), true)));
        assert_eq!(got[1], Ok((200, b"Wikipedia".to_vec(), true)));
        assert_eq!(got[2], Ok((304, Vec::new(), true)));
        assert_eq!(got[3], Ok((200, b"until the end".to_vec(), false)));
    }

    #[test]
    fn decodes_gzip_bodies() {
        let gz: &[u8] = &[
            31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 179, 41, 176, 75, 175, 202, 44, 80, 72, 202, 79,
            169, 180, 209, 47, 176, 3, 0, 82, 122, 105, 209, 16, 0, 0, 0,
        ];
        let mut wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
            gz.len()
        )
        .into_bytes();
        wire.extend_from_slice(gz);
        assert_eq!(
            responses(&wire)[0],
            Ok((200, b"<p>gzip body</p>".to_vec(), true))
        );
    }

    #[test]
    fn rejects_broken_framing() {
        let short = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc";
        assert!(responses(short)[0].is_err(), "body shorter than its length");
        let huge = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert!(
            responses(huge.as_bytes())[0].is_err(),
            "over the body limit"
        );
        let bad_chunk = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n";
        assert!(responses(bad_chunk)[0].is_err());
        assert!(
            responses(b"").is_empty(),
            "a closed connection is retryable, not a response"
        );
    }
}
