use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

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
    pub fn parse(input: &str) -> Result<Url, String> {
        let input = input.trim();
        let (scheme, rest) = input
            .split_once("://")
            .ok_or_else(|| format!("missing scheme: {input}"))?;
        let scheme = scheme.to_ascii_lowercase();

        if scheme == "file" {
            return Ok(Url {
                scheme,
                host: String::new(),
                port: 0,
                path: rest.to_string(),
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

        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().map_err(|_| format!("bad port: {p}"))?),
            None => (authority, default_port),
        };
        if host.is_empty() {
            return Err(format!("empty host: {input}"));
        }
        Ok(Url {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
            path: normalize_path(&path),
        })
    }

    /// Resolve `href` (absolute, protocol-relative, root-relative or relative) against this URL.
    pub fn join(&self, href: &str) -> Result<Url, String> {
        let href = href.trim();
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
        url.path = normalize_path(&url.path);
        Ok(url)
    }
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
    pub body: Vec<u8>,
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

/// GET a URL, following redirects. A request that fails on a reused or reset connection is
/// retried once on a fresh one.
pub fn fetch(url: &Url) -> Result<Response, String> {
    let mut url = url.clone();
    for _ in 0..MAX_REDIRECTS {
        let resp = match fetch_once(&url) {
            Err(FetchError::Retryable(_)) => fetch_once(&url)?,
            other => other?,
        };
        if (300..400).contains(&resp.status) {
            if let Some(location) = resp.header("location") {
                url = url.join(location)?;
                continue;
            }
        }
        return Ok(resp);
    }
    Err(format!("too many redirects ending at {url}"))
}

fn fetch_once(url: &Url) -> Result<Response, FetchError> {
    if url.scheme == "file" {
        let body = std::fs::read(&url.path).map_err(|e| format!("{}: {e}", url.path))?;
        return Ok(Response {
            url: url.clone(),
            status: 200,
            headers: Vec::new(),
            body,
        });
    }
    let key = (url.scheme.clone(), url.host.clone(), url.port);
    let (mut conn, reused) = match take_idle(&key) {
        Some(conn) => (conn, true),
        None => (Conn::open(url)?, false),
    };
    match request(&mut conn, url) {
        Ok((resp, reusable)) => {
            if reusable {
                put_idle(key, conn);
            }
            Ok(resp)
        }
        // A pooled connection the server already closed: not the server's answer.
        Err(FetchError::Retryable(_)) if reused => {
            let mut conn = Conn::open(url)?;
            let (resp, reusable) = request(&mut conn, url)?;
            if reusable {
                put_idle(key, conn);
            }
            Ok(resp)
        }
        Err(e) => Err(e),
    }
}

fn request(conn: &mut Conn, url: &Url) -> Result<(Response, bool), FetchError> {
    let host_header = if Some(url.port) == default_port(&url.scheme) {
        url.host.clone()
    } else {
        format!("{}:{}", url.host, url.port)
    };
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: {USER_AGENT}\r\n\
         Accept: text/html,*/*;q=0.8\r\nAccept-Encoding: gzip, deflate\r\n\
         Connection: keep-alive\r\n\r\n",
        url.path
    );
    conn.stream
        .write_all(request.as_bytes())
        .map_err(io_error)?;
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
        body: raw.body,
    };
    let encoding = resp.header("content-encoding").map(str::to_ascii_lowercase);
    resp.body = match encoding.as_deref().map(str::trim) {
        Some("gzip" | "x-gzip") => {
            flate::gunzip(&resp.body, MAX_BODY).map_err(|e| e.to_string())?
        }
        // "deflate" is meant to be zlib-wrapped, but some servers send raw DEFLATE.
        Some("deflate") => flate::zlib_decompress(&resp.body, MAX_BODY)
            .or_else(|_| flate::inflate(&resp.body, MAX_BODY))
            .map_err(|e| e.to_string())?,
        _ => resp.body,
    };
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

fn tls_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
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

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

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
